// 控制器配置面板(07 §GUI):经 Zenoh 编辑控制器 launch.yaml。
// 流程:连接 → 发现控制器(<cid>/info) → 选中 → 只读拉取(<cid>/config)→ 编辑 →
//   校验(rpc/config/validate:errors 内联 + critical_changes 红色 diff)→
//   保存(set apply=false)→ 应用(set apply=true;确认框列出受影响 robots + 红线复述)。
// 乐观锁 expect_sha256 全程带上;定期 get 比对 sha 检测外部(ssh)修改 → 提示 reload。
// 后端契约见 src-tauri/src/zenoh_config.rs。
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { App as AntdApp, Alert, Button, Input, Modal, Select, Switch, Tag, Tooltip, Typography } from "antd";
import { ReloadOutlined } from "@ant-design/icons";
import { api, errMsg } from "../api";
import { useI18n } from "../i18n";
import type { ApiVersion, ConfigGetDto, ConfigValidateResult, ControllerInfo, CriticalChange } from "../types";
import "./ControllerConfigPanel.css";

// 本 GUI 支持的 config schema 主版本。控制器上报的 schema_version.major 更高 → 只读(沿用 01 版本规则)。
const SUPPORTED_SCHEMA_MAJOR = 1;
// 外部修改检测轮询间隔。
const EXTERNAL_POLL_MS = 4000;

function fmtVersion(v: ApiVersion | null | undefined): string {
  if (!v) return "—";
  return `${v.major}.${v.minor}.${v.patch}`;
}

function fmtMtime(unix: number): string {
  if (!unix) return "—";
  try {
    return new Date(unix * 1000).toLocaleString();
  } catch {
    return String(unix);
  }
}

// 待确认操作(apply 或含红线的 save)。
interface Pending {
  apply: boolean;
  critical: CriticalChange[];
  robots: string[]; // 受影响机器人展示名(来自选中控制器的 robots)
}

export function ControllerConfigPanel() {
  const { message, modal } = AntdApp.useApp();
  const { t } = useI18n();

  const [endpoint, setEndpoint] = useState("");
  const [connected, setConnected] = useState(false);
  const [busyConn, setBusyConn] = useState(false);
  const [controllers, setControllers] = useState<ControllerInfo[]>([]);
  const [cid, setCid] = useState<string | null>(null);

  const [cfg, setCfg] = useState<ConfigGetDto | null>(null);
  const [yaml, setYaml] = useState("");
  const [loadedYaml, setLoadedYaml] = useState("");
  const [loadedSha, setLoadedSha] = useState("");

  const [validateResult, setValidateResult] = useState<ConfigValidateResult | null>(null);
  const [external, setExternal] = useState<ConfigGetDto | null>(null);
  const [force, setForce] = useState(false);
  const [action, setAction] = useState<null | "validate" | "save" | "apply" | "restart">(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [pendingLoading, setPendingLoading] = useState(false);

  // 供轮询/异步回调读取最新值,避免闭包过期。
  const cidRef = useRef<string | null>(null);
  const loadedShaRef = useRef("");
  cidRef.current = cid;
  loadedShaRef.current = loadedSha;

  const dirty = yaml !== loadedYaml;
  const selected = controllers.find((c) => c.cid === cid) ?? null;
  const schema = cfg?.schema_version ?? null;
  const readOnly = !!schema && schema.major > SUPPORTED_SCHEMA_MAJOR;
  const canEdit = connected && !!cid && !!cfg && !readOnly;
  const robotLabels = useMemo(
    () => (selected?.robots ?? []).map((r) => `${r.model || r.kind_name} (${r.robot_index})`),
    [selected]
  );

  // 断开时清理连接。
  useEffect(
    () => () => {
      api.configDisconnect().catch(() => {});
    },
    []
  );

  const applyLoaded = useCallback((g: ConfigGetDto) => {
    setCfg(g);
    setYaml(g.yaml);
    setLoadedYaml(g.yaml);
    setLoadedSha(g.sha256);
    setValidateResult(null);
    setExternal(null);
  }, []);

  const loadConfig = useCallback(
    async (targetCid: string) => {
      try {
        const g = await api.configGet(targetCid);
        applyLoaded(g);
      } catch (e) {
        message.error(errMsg(e));
      }
    },
    [applyLoaded, message]
  );

  const connect = useCallback(async () => {
    setBusyConn(true);
    try {
      await api.configConnect(endpoint.trim());
      setConnected(true);
      message.success(t("cfgConnected"));
      let list = await api.configDiscover();
      if (list.length === 0) {
        await new Promise((r) => setTimeout(r, 900));
        list = await api.configDiscover();
      }
      setControllers(list);
      const first = list[0]?.cid ?? null;
      setCid(first);
      if (first) await loadConfig(first);
      if (list.length === 0) message.warning(t("cfgNoController"));
    } catch (e) {
      message.error(errMsg(e));
    } finally {
      setBusyConn(false);
    }
  }, [endpoint, loadConfig, message, t]);

  const disconnect = useCallback(async () => {
    try {
      await api.configDisconnect();
    } catch {
      /* ignore */
    }
    setConnected(false);
    setControllers([]);
    setCid(null);
    setCfg(null);
    setYaml("");
    setLoadedYaml("");
    setLoadedSha("");
    setValidateResult(null);
    setExternal(null);
  }, []);

  const discover = useCallback(async () => {
    try {
      const list = await api.configDiscover();
      setControllers(list);
      if (!cid && list[0]) {
        setCid(list[0].cid);
        await loadConfig(list[0].cid);
      }
      if (list.length === 0) message.warning(t("cfgNoController"));
    } catch (e) {
      message.error(errMsg(e));
    }
  }, [cid, loadConfig, message, t]);

  const onSelectController = useCallback(
    (next: string) => {
      setCid(next);
      loadConfig(next);
    },
    [loadConfig]
  );

  const reload = useCallback(() => {
    if (cid) loadConfig(cid);
  }, [cid, loadConfig]);

  // 定期 get 比对 sha,检测外部(ssh)修改。有未加载差异 → 挂横幅提示 reload。
  useEffect(() => {
    if (!connected || !cid) return;
    let alive = true;
    const tick = async () => {
      const target = cidRef.current;
      if (!target) return;
      try {
        const g = await api.configGet(target);
        if (!alive || cidRef.current !== target) return;
        if (g.sha256 !== loadedShaRef.current) setExternal(g);
        else setExternal(null);
      } catch {
        /* transient */
      }
    };
    const h = window.setInterval(tick, EXTERNAL_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(h);
    };
  }, [connected, cid]);

  const validate = useCallback(async () => {
    if (!cid) return;
    setAction("validate");
    try {
      const r = await api.configValidate(cid, yaml);
      setValidateResult(r);
      if (r.ok) message.success(t("cfgValidateOk"));
      else message.error(t("cfgValidateFail"));
    } catch (e) {
      message.error(errMsg(e));
    } finally {
      setAction(null);
    }
  }, [cid, yaml, message, t]);

  // 失败后判定是否 sha 冲突(409):重取当前文件,若指纹与 loadedSha 不符 → 外部修改。
  const detectConflict = useCallback(async (): Promise<boolean> => {
    const target = cidRef.current;
    if (!target) return false;
    try {
      const g = await api.configGet(target);
      if (g.sha256 !== loadedShaRef.current) {
        setExternal(g);
        return true;
      }
    } catch {
      /* ignore */
    }
    return false;
  }, []);

  // 实际写入(已过校验 + 已确认)。confirm/force 由调用方给。
  const performSet = useCallback(
    async (apply: boolean, confirm: boolean, forceFlag: boolean): Promise<boolean> => {
      if (!cid) return false;
      try {
        const r = await api.configSet(cid, yaml, loadedShaRef.current, apply, confirm, forceFlag);
        if (r.ok) {
          setValidateResult({ ok: true, errors: [], critical_changes: [] });
          setLoadedYaml(yaml);
          setLoadedSha(r.sha256);
          setExternal(null);
          setCfg((prev) => (prev ? { ...prev, sha256: r.sha256, yaml } : prev));
          if (apply) {
            message.success(
              r.robots.length ? `${t("cfgApplied")}: ${r.robots.join(", ")}` : t("cfgApplied")
            );
          } else {
            message.success(t("cfgSaved"));
          }
          // 回读一次刷新元数据(recovery_mode / mtime / path);写入是逐字节的,故编辑器内容不变。
          api
            .configGet(cid)
            .then((g) => {
              if (cidRef.current === cid) {
                setCfg(g);
                setLoadedSha(g.sha256);
              }
            })
            .catch(() => {});
          return true;
        }
        // 未成功:内联展示服务端 errors + critical。
        setValidateResult({ ok: false, errors: r.errors, critical_changes: r.critical_changes });
        const conflict = await detectConflict();
        if (conflict) message.error(t("cfgConflictShort"));
        else message.error(apply ? t("cfgApplyFail") : t("cfgSaveFail"));
        return false;
      } catch (e) {
        message.error(errMsg(e));
        return false;
      }
    },
    [cid, yaml, detectConflict, message, t]
  );

  // 保存 / 应用统一入口:先干跑校验(非法绝不写)→ 需要确认则开确认框,否则直接写。
  const submit = useCallback(
    async (apply: boolean) => {
      if (!cid) return;
      setAction(apply ? "apply" : "save");
      try {
        const r = await api.configValidate(cid, yaml);
        setValidateResult(r);
        if (!r.ok) {
          message.error(t("cfgValidateFail"));
          return;
        }
        const critical = r.critical_changes;
        // apply 必须确认(全部 robots 会重启);save 仅在有红线时确认。
        if (apply || critical.length > 0) {
          setPending({ apply, critical, robots: robotLabels });
        } else {
          await performSet(false, false, force);
        }
      } catch (e) {
        message.error(errMsg(e));
      } finally {
        setAction(null);
      }
    },
    [cid, yaml, robotLabels, force, performSet, message, t]
  );

  const confirmPending = useCallback(async () => {
    if (!pending) return;
    setPendingLoading(true);
    // 完成后统一关闭确认框:成功走 message,失败由内联 errors / 冲突横幅呈现。
    await performSet(pending.apply, true, force);
    setPendingLoading(false);
    setPending(null);
  }, [pending, force, performSet]);

  // 单独"应用"已保存的文件(不重写,直接重启全部子进程)。同样列出受影响 robots 后再执行。
  const doRestart = useCallback(async () => {
    if (!cid) return;
    setAction("restart");
    try {
      const r = await api.configRestart(cid, true, force);
      if (r.ok) message.success(r.robots.length ? `${t("cfgRestartDone")}: ${r.robots.join(", ")}` : t("cfgRestartDone"));
      else message.error(t("cfgRestartFail"));
    } catch (e) {
      message.error(errMsg(e));
    } finally {
      setAction(null);
    }
  }, [cid, force, message, t]);

  const restart = useCallback(() => {
    if (!cid) return;
    modal.confirm({
      title: t("cfgConfirmApplyTitle"),
      okText: t("cfgConfirmOk"),
      cancelText: t("cfgConfirmCancel"),
      okButtonProps: { danger: true },
      content: (
        <div>
          <p>{t("cfgConfirmApplyBody")}</p>
          <strong>{t("cfgAffectedRobots")}</strong>
          {robotLabels.length ? (
            <ul className="cfg-robot-list">
              {robotLabels.map((r) => (
                <li key={r}>{r}</li>
              ))}
            </ul>
          ) : (
            <p style={{ color: "#8a93a3" }}>{t("cfgNoRobots")}</p>
          )}
        </div>
      ),
      onOk: doRestart,
    });
  }, [cid, robotLabels, modal, doRestart, t]);

  return (
    <div className="cfg-panel">
      <section className="cfg-connect-panel">
        <label className="cfg-field cfg-field--endpoint">
          <span>{t("zEndpoint")}</span>
          <Input
            value={endpoint}
            disabled={connected}
            placeholder={t("zEndpointHint")}
            onChange={(e) => setEndpoint(e.target.value)}
          />
        </label>
        <div className="cfg-connect-panel__actions">
          {connected ? (
            <Button onClick={disconnect}>{t("cfgDisconnect")}</Button>
          ) : (
            <Button type="primary" loading={busyConn} onClick={connect}>
              {t("cfgConnect")}
            </Button>
          )}
          <Button disabled={!connected} onClick={discover}>
            {t("cfgDiscover")}
          </Button>
        </div>
        <div className="cfg-discovery">
          <Typography.Text type="secondary">
            {t("cfgFound")}: {controllers.length}
          </Typography.Text>
          <Select
            className="cfg-discovery__select"
            value={cid ?? undefined}
            onChange={onSelectController}
            placeholder={t("cfgSelectController")}
            disabled={!connected || controllers.length === 0}
            options={controllers.map((c) => ({
              value: c.cid,
              label: `${c.controller_id || c.cid}${c.fw_version ? ` · ${c.fw_version}` : ""}`,
            }))}
          />
          <Tooltip title={t("cfgReload")}>
            <Button icon={<ReloadOutlined />} disabled={!cid} onClick={reload} />
          </Tooltip>
          <span className="cfg-dock-status">
            {connected ? <Tag color="blue">{t("cfgConnected")}</Tag> : <Tag>{t("cfgDisconnected")}</Tag>}
            {selected && (
              <Tag>
                {t("cfgRobotsOnController")}: {selected.robots.length}
              </Tag>
            )}
          </span>
        </div>
      </section>

      {cfg?.recovery_mode && (
        <Alert type="error" showIcon message={t("cfgRecoveryTitle")} description={t("cfgRecoveryDesc")} />
      )}

      {external && (
        <Alert
          type="warning"
          showIcon
          message={t("cfgConflictTitle")}
          description={t("cfgConflictDesc")}
          action={
            <Button size="small" danger onClick={reload}>
              {t("cfgReloadDiscard")}
            </Button>
          }
        />
      )}

      {readOnly && (
        <Alert type="info" showIcon message={t("cfgReadonlyTitle")} description={t("cfgReadonlyDesc")} />
      )}

      {cfg && (
        <section className="cfg-meta">
          <MetaItem label={t("cfgPath")} value={cfg.path || "—"} mono wide />
          <MetaItem label={t("cfgSha")} value={cfg.sha256 ? cfg.sha256.slice(0, 16) + "…" : "—"} mono title={cfg.sha256} />
          <MetaItem label={t("cfgMtime")} value={fmtMtime(cfg.mtime_unix)} />
          <MetaItem label={t("cfgSchemaVersion")} value={fmtVersion(schema)} mono />
        </section>
      )}

      <section className="cfg-editor-card">
        <div className="cfg-editor-card__head">
          <div>
            <h2>{t("cfgEditorTitle")}</h2>
            <Typography.Text type="secondary">{t("cfgEditorHint")}</Typography.Text>
          </div>
          <div className="cfg-editor-card__flags">
            {dirty ? <Tag color="gold">{t("cfgModified")}</Tag> : cfg ? <Tag color="green">{t("cfgClean")}</Tag> : null}
            <Tooltip title={t("cfgForceHint")}>
              <span className="cfg-force">
                <Typography.Text type="secondary">{t("cfgForce")}</Typography.Text>
                <Switch size="small" checked={force} onChange={setForce} disabled={!canEdit} />
              </span>
            </Tooltip>
          </div>
        </div>

        <YamlEditor value={yaml} onChange={setYaml} readOnly={!canEdit} disabled={!cfg} />

        <div className="cfg-actions">
          <Button loading={action === "validate"} disabled={!cid || !cfg || action !== null} onClick={validate}>
            {t("cfgValidate")}
          </Button>
          <Button
            type="default"
            loading={action === "save"}
            disabled={!canEdit || !dirty || action !== null}
            onClick={() => submit(false)}
          >
            {t("cfgSave")}
          </Button>
          <Button
            type="primary"
            danger
            loading={action === "apply"}
            disabled={!canEdit || action !== null}
            onClick={() => submit(true)}
          >
            {t("cfgApply")}
          </Button>
          <Tooltip title={t("cfgApplyNoChangesHint")}>
            <Button
              loading={action === "restart"}
              disabled={!connected || !cid || action !== null}
              onClick={restart}
            >
              {t("cfgRestart")}
            </Button>
          </Tooltip>
        </div>
      </section>

      {validateResult && <ValidationResults result={validateResult} />}

      <Modal
        open={!!pending}
        title={pending?.apply ? t("cfgConfirmApplyTitle") : t("cfgConfirmSaveTitle")}
        okText={t("cfgConfirmOk")}
        cancelText={t("cfgConfirmCancel")}
        okButtonProps={{ danger: true, loading: pendingLoading }}
        cancelButtonProps={{ disabled: pendingLoading }}
        onOk={confirmPending}
        onCancel={() => (pendingLoading ? null : setPending(null))}
        maskClosable={false}
      >
        {pending?.apply && (
          <>
            <Typography.Paragraph>{t("cfgConfirmApplyBody")}</Typography.Paragraph>
            <Typography.Text strong>{t("cfgAffectedRobots")}</Typography.Text>
            {pending.robots.length ? (
              <ul className="cfg-robot-list">
                {pending.robots.map((r) => (
                  <li key={r}>{r}</li>
                ))}
              </ul>
            ) : (
              <Typography.Paragraph type="secondary">{t("cfgNoRobots")}</Typography.Paragraph>
            )}
          </>
        )}
        {pending && pending.critical.length > 0 && (
          <>
            <Typography.Text strong type="danger">
              {t("cfgConfirmRecite")}
            </Typography.Text>
            <div className="cfg-critical-list">
              {pending.critical.map((c, i) => (
                <CriticalRow key={`${c.robot_id}-${c.field}-${i}`} c={c} />
              ))}
            </div>
          </>
        )}
      </Modal>
    </div>
  );
}

function MetaItem({
  label,
  value,
  mono,
  wide,
  title,
}: {
  label: string;
  value: string;
  mono?: boolean;
  wide?: boolean;
  title?: string;
}) {
  return (
    <div className={`cfg-meta-item${wide ? " cfg-meta-item--wide" : ""}`}>
      <span className="cfg-meta-item__label">{label}</span>
      <strong className={`cfg-meta-item__value${mono ? " cfg-mono" : ""}`} title={title ?? value}>
        {value}
      </strong>
    </div>
  );
}

function ValidationResults({ result }: { result: ConfigValidateResult }) {
  const { t } = useI18n();
  return (
    <section className="cfg-results">
      {result.errors.length > 0 ? (
        <div className="cfg-result-block">
          <Typography.Text strong type="danger">
            {t("cfgErrors")} ({result.errors.length})
          </Typography.Text>
          <ul className="cfg-error-list">
            {result.errors.map((e, i) => (
              <li key={i}>{e}</li>
            ))}
          </ul>
        </div>
      ) : (
        <Tag color={result.ok ? "green" : "default"}>{result.ok ? t("cfgValidateOk") : t("cfgNoErrors")}</Tag>
      )}

      {result.critical_changes.length > 0 && (
        <div className="cfg-result-block">
          <Typography.Text strong type="danger">
            {t("cfgCriticalTitle")} ({result.critical_changes.length})
          </Typography.Text>
          <Typography.Paragraph type="secondary" className="cfg-critical-hint">
            {t("cfgCriticalHint")}
          </Typography.Paragraph>
          <div className="cfg-critical-list">
            {result.critical_changes.map((c, i) => (
              <CriticalRow key={`${c.robot_id}-${c.field}-${i}`} c={c} />
            ))}
          </div>
        </div>
      )}
    </section>
  );
}

function CriticalRow({ c }: { c: CriticalChange }) {
  return (
    <div className="cfg-critical-row">
      <span className="cfg-critical-row__id">{c.robot_id}</span>
      <code className="cfg-critical-row__field">{c.field}</code>
      <span className="cfg-critical-row__diff">
        <span className="cfg-diff-old">{c.old || "∅"}</span>
        <span className="cfg-diff-arrow">→</span>
        <span className="cfg-diff-new">{c.new || "∅"}</span>
      </span>
    </div>
  );
}

// 轻量 YAML 编辑器:等宽 textarea + 行号侧栏(scrollTop 同步)。不引入外部依赖。
function YamlEditor({
  value,
  onChange,
  readOnly,
  disabled,
}: {
  value: string;
  onChange: (v: string) => void;
  readOnly: boolean;
  disabled: boolean;
}) {
  const taRef = useRef<HTMLTextAreaElement>(null);
  const gutterRef = useRef<HTMLDivElement>(null);
  const lineCount = useMemo(() => Math.max(1, value.split("\n").length), [value]);

  const syncScroll = useCallback(() => {
    if (gutterRef.current && taRef.current) {
      gutterRef.current.scrollTop = taRef.current.scrollTop;
    }
  }, []);

  return (
    <div className={`cfg-editor${disabled ? " cfg-editor--disabled" : ""}`}>
      <div className="cfg-editor__gutter" ref={gutterRef} aria-hidden="true">
        {Array.from({ length: lineCount }, (_, i) => (
          <div key={i}>{i + 1}</div>
        ))}
      </div>
      <textarea
        ref={taRef}
        className="cfg-editor__ta"
        value={value}
        spellCheck={false}
        wrap="off"
        readOnly={readOnly}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value)}
        onScroll={syncScroll}
      />
    </div>
  );
}
