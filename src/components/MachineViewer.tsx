// 常驻 3D 场景(M2,13 §5):controller 级——散装布局(无 machine 段):各 robot 摆地面,
// 按 robot_index 排序网格排布(间距可配,默认 2m);臂用机器人级整机 URDF(自带 EE),
// 被绑 EE 不再单独摆(同 cid 有 assembled 臂 ⇒ 隐藏该 cid 的 ee 节点;精确 ee↔arm 映射 TODO)。
// 关节驱动:全 kind joint_state 聚合(SceneRobot.q × joint_names);EE 关节以 ee_ 前缀写进
// 臂的整机模型(mimic 从动由 urdf-loader 0.13 原生联动)。无 URDF 的 robot(如 base)画占位盒。
// M3:machine 段拼接(MountEdge:child 挂 parent 的 mount link + offset;parent URDF/link 缺失 →
// 告警 + 该分支散装,不拼错不猜,13 §3)、选中聚焦(其余 ghost 半透明/隐藏可切)、3D 点击选中。

import { useEffect, useRef, useState } from "react";
import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import type { URDFRobot } from "urdf-loader";
import { api } from "../api";
import type { MountEdge, SceneRobot } from "../types";
import {
  beginUrdfLoad,
  disposeObject3D,
  isUrdfAbort,
  urdfRetryDelayMs,
  UrdfLoadError,
  type UrdfLoadJob,
} from "../urdfModelLoader";

interface Props {
  robots: SceneRobot[];       // ee_scene 轮询(~30Hz)
  selected: string | null;    // 选中 robot prefix(高亮)
  spacing: number;            // 散装网格间距 m
  machines: Record<string, MountEdge[]>; // cid → 挂载边(M3;空 = 散装)
  focusMode: "ghost" | "hide" | "off";   // 选中聚焦:其余半透明/隐藏/不处理
  onSelect?: (prefix: string) => void;   // 3D 点击选中
  height?: number;
}

type Slot = {
  group: THREE.Group;               // 网格位安放点
  robot: URDFRobot | null;          // 已加载的 URDF 模型(null=占位盒/加载中)
  assembled: boolean;               // 臂:整机(含 EE)
  kind: string;
  placeholder: THREE.Mesh | null;
  loading: boolean;
  lastFetch: number;                // 上次 URDF 拉取时刻(ms;臂未拼装时周期重拉)
  sourceXml: string | null;         // 当前成功模型的精确 XML(同 assembled 但 XML 变化也重载)
  loadGeneration: number;           // 丢弃 Tauri/STL 迟到结果
  loadJob: UrdfLoadJob | null;      // 当前可取消的 STL 作业(Tauri invoke 本身不可取消)
  error: UrdfLoadError | null;
  failureCount: number;
  nextRetryAt: number | null;
  missingSince: number | null;
  hlMode: "off" | "full" | "ee";    // 高亮:整机 / 仅 ee_ 子树(被绑 EE 选中时)
  mountedTo: string | null;         // 已挂到的 "parentPrefix/link"(M3 拼接;null=散装在地面)
  dimMode: "none" | "all" | "body"; // 聚焦淡化:全部 / 仅臂身(ee_ 子树保亮)
  selfHidden: boolean;              // hide 聚焦时仅隐藏 ancestor 自有 mesh，保留其 child transform
  warned: boolean;                  // 挂载失败告警只打一次
};

const HIGHLIGHT = new THREE.Color(0x2a6fbb);

type LoadIssue = {
  prefix: string;
  message: string;
  failureCount: number;
  retrying: boolean;
  keepingOldModel: boolean;
};

export function MachineViewer({ robots, selected, spacing, machines, focusMode, onSelect, height = 340 }: Props) {
  const mountRef = useRef<HTMLDivElement>(null);
  const controlsRef = useRef<OrbitControls | null>(null);
  const slotsRef = useRef<Map<string, Slot>>(new Map());
  const worldRef = useRef<THREE.Group | null>(null);
  const propsRef = useRef({ robots, selected, spacing, machines, focusMode });
  propsRef.current = { robots, selected, spacing, machines, focusMode };
  const onSelectRef = useRef(onSelect);
  onSelectRef.current = onSelect;
  const cameraRef = useRef<THREE.PerspectiveCamera | null>(null);
  const [loadIssues, setLoadIssues] = useState<LoadIssue[]>([]);

  function publishLoadIssues() {
    const issues: LoadIssue[] = [];
    slotsRef.current.forEach((slot, prefix) => {
      if (!slot.error) return;
      issues.push({
        prefix,
        message: slot.error.message,
        failureCount: slot.failureCount,
        retrying: slot.loading || slot.nextRetryAt != null,
        keepingOldModel: slot.robot != null,
      });
    });
    issues.sort((a, b) => a.prefix.localeCompare(b.prefix));
    setLoadIssues(issues);
  }

  function setPlaceholderFailed(slot: Slot, failed: boolean) {
    if (!slot.placeholder || Array.isArray(slot.placeholder.material)) return;
    (slot.placeholder.material as THREE.MeshPhongMaterial).color.set(failed ? 0x8b4545 : 0x555c66);
  }

  /** Parent 模型替换/删除前，把挂在其 link 下的外部 slot 所有权移回 world。 */
  function detachMountedChildren(parentPrefix: string) {
    const world = worldRef.current;
    if (!world) return;
    slotsRef.current.forEach((child, childPrefix) => {
      if (childPrefix === parentPrefix || !child.mountedTo?.startsWith(`${parentPrefix}/`)) return;
      world.add(child.group);
      child.mountedTo = null;
    });
  }

  function forEachOwnedMesh(slot: Slot, callback: (mesh: THREE.Mesh) => void) {
    const visit = (object: THREE.Object3D) => {
      if (object !== slot.group && typeof object.userData.prefix === "string") return;
      if ((object as THREE.Mesh).isMesh) callback(object as THREE.Mesh);
      object.children.forEach(visit);
    };
    visit(slot.group);
  }

  function setOwnedMeshesVisible(slot: Slot, visible: boolean) {
    forEachOwnedMesh(slot, (mesh) => { mesh.visible = visible; });
  }

  useEffect(() => {
    const mount = mountRef.current!;
    const W = mount.clientWidth || 800;
    const scene = new THREE.Scene();
    scene.background = new THREE.Color(0x1a1d23);
    const camera = new THREE.PerspectiveCamera(50, W / height, 0.01, 200);
    cameraRef.current = camera;
    camera.position.set(2.2, -2.6, 1.8);
    camera.up.set(0, 0, 1); // URDF Z-up
    const renderer = new THREE.WebGLRenderer({ antialias: true });
    renderer.setSize(W, height);
    renderer.setPixelRatio(window.devicePixelRatio);
    mount.appendChild(renderer.domElement);
    const controls = new OrbitControls(camera, renderer.domElement);
    controls.target.set(0, 0, 0.2);
    controlsRef.current = controls;

    // 3D 点击选中:pointerdown/up 位移 <6px 视为点击(区别于 orbit 拖拽)→ raycast → 祖先链找 slot 组
    let downXY: [number, number] | null = null;
    const onDown = (e: PointerEvent) => { downXY = [e.clientX, e.clientY]; };
    const onUp = (e: PointerEvent) => {
      if (!downXY) return;
      const moved = Math.hypot(e.clientX - downXY[0], e.clientY - downXY[1]);
      downXY = null;
      if (moved > 6) return;
      const rect = renderer.domElement.getBoundingClientRect();
      const ndc = new THREE.Vector2(
        ((e.clientX - rect.left) / rect.width) * 2 - 1,
        -((e.clientY - rect.top) / rect.height) * 2 + 1,
      );
      const ray = new THREE.Raycaster();
      ray.setFromCamera(ndc, camera);
      const hits = ray.intersectObjects(world.children, true);
      for (const h of hits) {
        // 命中 ee_ 子树(整机臂模型内)→ 选中被绑 EE 本体;否则选中所属 robot
        let hitEe = false;
        let o: THREE.Object3D | null = h.object;
        while (o) {
          if (o.name.startsWith("ee_")) hitEe = true;
          const pfx = (o.userData as { prefix?: string }).prefix;
          if (pfx) {
            if (hitEe) {
              const { robots } = propsRef.current;
              const host = robots.find((r) => r.prefix === pfx);
              const ee = host && robots.find((r) => r.kind_name === "ee" && r.cid === host.cid);
              if (ee) { onSelectRef.current?.(ee.prefix); return; }
            }
            onSelectRef.current?.(pfx);
            return;
          }
          o = o.parent;
        }
      }
    };
    renderer.domElement.addEventListener("pointerdown", onDown);
    renderer.domElement.addEventListener("pointerup", onUp);

    scene.add(new THREE.AmbientLight(0xffffff, 0.75));
    const dir = new THREE.DirectionalLight(0xffffff, 0.8);
    dir.position.set(2, 2, 4);
    scene.add(dir);
    const grid = new THREE.GridHelper(12, 60, 0x444444, 0x2a2a2a).rotateX(Math.PI / 2);
    (grid.material as THREE.Material).transparent = true;
    (grid.material as THREE.Material).opacity = 0.3;
    scene.add(grid);

    const world = new THREE.Group();
    scene.add(world);
    worldRef.current = world;

    // 每帧:布局 + 关节驱动 + 高亮(数据从 propsRef 拉,避免 effect 重建 three 场景)
    let raf = 0;
    const animate = () => {
      applyFrame();
      controls.update();
      renderer.render(scene, camera);
      raf = requestAnimationFrame(animate);
    };
    animate();
    const onResize = () => {
      const w = mount.clientWidth || 800;
      camera.aspect = w / height; camera.updateProjectionMatrix(); renderer.setSize(w, height);
    };
    window.addEventListener("resize", onResize);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", onResize);
      renderer.domElement.removeEventListener("pointerdown", onDown);
      renderer.domElement.removeEventListener("pointerup", onUp);
      controls.dispose();
      grid.dispose();
      renderer.dispose();
      if (renderer.domElement.parentNode === mount) mount.removeChild(renderer.domElement);
      // 先拆开所有 slot ownership，避免 parent robot traverse 到 mounted child 后重复释放。
      slotsRef.current.forEach((slot) => {
        slot.loadGeneration += 1;
        slot.loadJob?.cancel();
        slot.group.removeFromParent();
      });
      slotsRef.current.forEach((s) => disposeSlot(s));
      slotsRef.current.clear();
      controlsRef.current = null;
      cameraRef.current = null;
      worldRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [height]);

  function disposeSlot(s: Slot) {
    s.loadGeneration += 1;
    s.loadJob?.cancel();
    s.loadJob = null;
    disposeObject3D(s.group);
    s.robot = null;
    s.placeholder = null;
  }

  async function loadUrdfInto(slot: Slot, prefix: string, kindName: string, manual = false) {
    if (slot.loading && !manual) return;
    if (manual) {
      slot.failureCount = 0;
      slot.nextRetryAt = null;
      slot.error = null;
      setPlaceholderFailed(slot, false);
    }
    const generation = ++slot.loadGeneration;
    slot.loadJob?.cancel();
    slot.loadJob = null;
    slot.loading = true;
    slot.lastFetch = performance.now();
    publishLoadIssues();

    try {
      const u = await api.consoleGetUrdf(prefix, kindName);
      if (generation !== slot.loadGeneration) return;
      if (!u || !u.xml) {
        // base 可以是纯占位节点；其余可视 robot 的 URDF 短暂未发布也必须进入
        // 统一重试，不能第一次返回 null 后永远停在灰盒。
        if (kindName === "base") {
          slot.loading = false;
          slot.error = null;
          slot.failureCount = 0;
          slot.nextRetryAt = null;
          setPlaceholderFailed(slot, false);
          publishLoadIssues();
          return;
        }
        throw new UrdfLoadError([
          { stage: "urdf-fetch", url: `${prefix}/urdf`, cause: new Error("controller 未返回 URDF resource") },
          { stage: "urdf-fetch", url: `${prefix}/${kindName}/urdf`, cause: new Error("controller 未返回 fallback URDF resource") },
        ]);
      }
      if (slot.robot && slot.assembled === u.assembled && slot.sourceXml === u.xml) {
        slot.loading = false;
        slot.error = null;
        slot.failureCount = 0;
        slot.nextRetryAt = null;
        publishLoadIssues();
        return;
      }

      const job = beginUrdfLoad({ source: { kind: "xml", xml: u.xml, label: `${prefix}/urdf` } });
      slot.loadJob = job;
      const { robot } = await job.promise;
      if (generation !== slot.loadGeneration) {
        disposeObject3D(robot);
        return;
      }

      // 直到所有 STL 成功才在同一 tick 里换模型；assembled 升级失败时旧 arm-only 仍可见。
      const oldRobot = slot.robot;
      if (oldRobot) detachMountedChildren(prefix);
      slot.group.add(robot);
      slot.robot = robot;
      slot.sourceXml = u.xml;
      slot.assembled = u.assembled;
      slot.hlMode = "off";
      slot.dimMode = "none";
      slot.selfHidden = false;
      if (oldRobot) disposeObject3D(oldRobot);
      if (slot.placeholder) {
        disposeObject3D(slot.placeholder);
        slot.placeholder = null;
      }
      slot.loading = false;
      slot.loadJob = null;
      slot.error = null;
      slot.failureCount = 0;
      slot.nextRetryAt = null;
      publishLoadIssues();
    } catch (error) {
      if (generation !== slot.loadGeneration || isUrdfAbort(error)) return;
      slot.loading = false;
      slot.loadJob = null;
      slot.failureCount += 1;
      slot.error = error instanceof UrdfLoadError
        ? error
        : new UrdfLoadError([{
            stage: "urdf-fetch",
            url: `${prefix}/urdf`,
            cause: error,
          }]);
      const delay = urdfRetryDelayMs(slot.failureCount);
      slot.nextRetryAt = delay == null ? null : performance.now() + delay;
      setPlaceholderFailed(slot, true);
      console.warn(`URDF load failed: ${prefix}`, slot.error);
      publishLoadIssues();
    }
  }

  /** o 是否在某 URDF 模型的 ee_ 子树里(被绑 EE 的可视化长在宿主臂的整机模型内)。 */
  function inEeSubtree(o: THREE.Object3D, stopAt: THREE.Object3D): boolean {
    let cur: THREE.Object3D | null = o;
    while (cur && cur !== stopAt) {
      if (cur.name.startsWith("ee_")) return true;
      cur = cur.parent;
    }
    return false;
  }

  function applyFrame() {
    const world = worldRef.current;
    if (!world) return;
    const { robots, selected, spacing, machines, focusMode } = propsRef.current;
    const slots = slotsRef.current;
    const now = performance.now();

    // 短暂发现抖动保留 5s；真正离场后清理 slot。assembled 后被吸收的 EE 仍在
    // robots 中，因此不会被误删。
    const presentPrefixes = new Set(robots.map((robot) => robot.prefix));
    let removedIssue = false;
    slots.forEach((slot, prefix) => {
      if (presentPrefixes.has(prefix)) {
        slot.missingSince = null;
      } else if (slot.missingSince == null) {
        slot.missingSince = now;
      } else if (now - slot.missingSince >= 5000) {
        detachMountedChildren(prefix);
        disposeSlot(slot);
        slots.delete(prefix);
        removedIssue ||= slot.error != null;
      }
    });
    if (removedIssue) publishLoadIssues();

    // 被绑 EE 隐藏:同 cid 存在 assembled 臂 ⇒ 该 cid 的 ee 不单独摆(13 §1;精确映射 TODO)。
    const assembledCids = new Set<string>();
    slots.forEach((s, prefix) => {
      if (s.assembled) {
        const r = robots.find((x) => x.prefix === prefix);
        if (r) assembledCids.add(r.cid);
      }
    });
    const visible = robots.filter((r) => !(r.kind_name === "ee" && assembledCids.has(r.cid)));

    // 建/删 slot
    const seen = new Set<string>();
    visible.forEach((r) => {
      seen.add(r.prefix);
      if (!slots.has(r.prefix)) {
        const group = new THREE.Group();
        // 占位盒(base 等无 URDF,或加载中):40cm 立方线框
        const box = new THREE.Mesh(
          new THREE.BoxGeometry(0.4, 0.4, 0.25),
          new THREE.MeshPhongMaterial({ color: 0x555c66, transparent: true, opacity: 0.6 }),
        );
        box.position.z = 0.125;
        group.add(box);
        world.add(group);
        const slot: Slot = {
          group,
          robot: null,
          assembled: false,
          kind: r.kind_name,
          placeholder: box,
          loading: false,
          lastFetch: 0,
          sourceXml: null,
          loadGeneration: 0,
          loadJob: null,
          error: null,
          failureCount: 0,
          nextRetryAt: null,
          missingSince: null,
          hlMode: "off",
          mountedTo: null,
          dimMode: "none",
          selfHidden: false,
          warned: false,
        };
        group.userData.prefix = r.prefix; // 3D 点击选中:命中网格向上找到 slot 组
        slots.set(r.prefix, slot);
        loadUrdfInto(slot, r.prefix, r.kind_name);
      }
    });
    // 视觉载体解析:被绑 EE 的可视化在宿主臂整机模型的 ee_ 子树里。判据与可见性过滤**同源**
    // (assembledCids)——不能看 slots.has:启动早期臂未拼装时 ee 曾短暂可见,会留下停在旧散装
    // 格位的幽灵 slot,按 slot 存在性判断会环绕那个隐形组(实测 bug:点爪后环绕到 base 附近)。
    let visualSelected = selected;
    let eeSubtree = false;
    if (selected) {
      const selRobot = robots.find((r) => r.prefix === selected);
      if (selRobot?.kind_name === "ee" && assembledCids.has(selRobot.cid)) {
        const host = robots.find((r) => r.kind_name === "arm" && r.cid === selRobot.cid && slots.get(r.prefix)?.assembled);
        if (host) { visualSelected = host.prefix; eeSubtree = true; }
      }
    }

    // hide 模式下 selected 可能挂在另一个 slot 的 link 下。Three 的 ancestor
    // visible=false 会连 selected 一起藏掉，所以保留 ancestor group，仅隐藏它自有 mesh。
    const focusAncestors = new Set<string>();
    if (focusMode === "hide" && visualSelected) {
      let cursor = robots.find((robot) => robot.prefix === visualSelected);
      const visited = new Set<string>();
      while (cursor && !visited.has(cursor.prefix)) {
        visited.add(cursor.prefix);
        const edge = (machines[cursor.cid] ?? []).find(
          (candidate) => `hexmeow/${cursor!.cid}/${candidate.child}` === cursor!.prefix,
        );
        if (!edge) break;
        const parentPrefix = `hexmeow/${cursor.cid}/${edge.parent}`;
        focusAncestors.add(parentPrefix);
        cursor = robots.find((robot) => robot.prefix === parentPrefix);
      }
    }

    slots.forEach((s, prefix) => {
      const inScene = seen.has(prefix);
      // 聚焦-隐藏:非"视觉载体"隐藏(被绑 EE 选中时宿主臂 = 载体,保住不藏)
      const focusHidden = focusMode === "hide" && !!visualSelected
        && prefix !== visualSelected && !focusAncestors.has(prefix);
      s.group.visible = inScene && !focusHidden; // 被绑 EE / 消失的 robot:隐藏但保留(再现时秒回)
      const hideSelf = focusMode === "hide" && focusAncestors.has(prefix) && prefix !== visualSelected;
      if (hideSelf !== s.selfHidden) {
        s.selfHidden = hideSelf;
        setOwnedMeshesVisible(s, !hideSelf);
      }
    });

    // ── M3 拼接:machine 边把 child 挂到 parent 的 mount link 下(offset = xyz+rpy,URDF 语义)。
    // parent URDF 未加载 / mount link 不存在 → 告警一次 + 该分支散装(13 §3:不拼错不猜)。
    const mounted = new Set<string>();
    visible.forEach((r) => {
      const s = slots.get(r.prefix)!;
      const edges = machines[r.cid] ?? [];
      const edge = edges.find((e) => `hexmeow/${r.cid}/${e.child}` === r.prefix);
      let want: string | null = null;
      let parentLinkObj: THREE.Object3D | null = null;
      if (edge) {
        const parentPrefix = `hexmeow/${r.cid}/${edge.parent}`;
        const ps = visible.some((candidate) => candidate.prefix === parentPrefix) ? slots.get(parentPrefix) : undefined;
        const linkObj = ps?.robot?.links?.[edge.parent_link];
        if (linkObj) {
          want = `${parentPrefix}/${edge.parent_link}`;
          parentLinkObj = linkObj;
          s.warned = false;
        }
        else if (!s.warned && ps?.robot) {
          s.warned = true;
          console.warn(`machine: ${r.prefix} 挂载点 ${edge.parent_link} 不在 ${parentPrefix} 的 URDF 里 → 散装回退(13 §3)`);
        }
      }
      const desiredParent = parentLinkObj ?? world;
      if (s.mountedTo !== want || s.group.parent !== desiredParent) {
        s.mountedTo = want;
        if (want && parentLinkObj) {
          parentLinkObj.add(s.group);
          s.group.position.set(edge!.xyz[0], edge!.xyz[1], edge!.xyz[2]);
          // URDF rpy(extrinsic XYZ)= three.js Euler 'ZYX' 语序:R = Rz(y)·Ry(p)·Rx(r)
          s.group.rotation.set(edge!.rpy[0], edge!.rpy[1], edge!.rpy[2], "ZYX");
        } else {
          worldRef.current!.add(s.group);
          s.group.rotation.set(0, 0, 0);
        }
      }
      if (want) mounted.add(r.prefix);
    });

    // 散装网格布局(仅未挂载的根;按 visible 顺序 = 后端已按 cid+robot_index 排序)
    const grounded = visible.filter((r) => !mounted.has(r.prefix));
    const n = grounded.length;
    const cols = Math.max(1, Math.ceil(Math.sqrt(n)));
    grounded.forEach((r, i) => {
      const s = slots.get(r.prefix)!;
      const col = i % cols, row = Math.floor(i / cols);
      s.group.position.set(col * spacing - ((cols - 1) * spacing) / 2, -row * spacing, 0);
    });

    // 关节驱动 + EE 关节写进臂的整机模型(ee_ 前缀;mimic 由 urdf-loader 联动)
    const byCid = new Map<string, SceneRobot[]>();
    robots.forEach((r) => { const a = byCid.get(r.cid) ?? []; a.push(r); byCid.set(r.cid, a); });
    visible.forEach((r) => {
      const s = slots.get(r.prefix);
      if (!s?.robot) return;
      r.joint_names.forEach((name, i) => {
        if (r.q[i] != null && s.robot!.joints[name]) s.robot!.setJointValue(name, r.q[i]);
      });
      if (r.kind_name === "arm" && s.assembled) {
        (byCid.get(r.cid) ?? []).filter((x) => x.kind_name === "ee").forEach((ee) => {
          ee.joint_names.forEach((name, i) => {
            const jn = `ee_${name}`;
            if (ee.q[i] != null && s.robot!.joints[jn]) s.robot!.setJointValue(jn, ee.q[i]);
          });
        });
      }
    });

    // 臂未拼装(EE 拼装比 GUI 首查晚就绪)→ 每 5s 重拉 URDF,拼好即替换成整机模型
    visible.forEach((r) => {
      const s = slots.get(r.prefix);
      if (!s || s.loading) return;
      if (s.error && s.nextRetryAt != null && now >= s.nextRetryAt) {
        s.nextRetryAt = null;
        void loadUrdfInto(s, r.prefix, r.kind_name);
      } else if (!s.error && s.kind === "arm" && !s.assembled && now - s.lastFetch > 5000) {
        loadUrdfInto(s, r.prefix, r.kind_name);
      }
    });

    // 视角跟随:orbit 目标平滑趋向选中 robot;被绑 EE → 环绕宿主臂模型里的 ee_base_link(爪的真实位置)
    const controls = controlsRef.current;
    if (controls) {
      const tgt = new THREE.Vector3(0, 0, 0.2);
      if (visualSelected) {
        const s = slots.get(visualSelected);
        if (s) {
          const eeLink = eeSubtree ? s.robot?.links?.["ee_base_link"] : null;
          if (eeLink) { eeLink.getWorldPosition(tgt); }
          else { s.group.getWorldPosition(tgt); tgt.z += 0.25; }
        }
      }
      controls.target.lerp(tgt, 0.08); // 平滑跟随,不瞬跳
    }

    // 聚焦-幽灵:非载体全淡化;被绑 EE 选中时宿主臂 = "仅臂身淡化"(ee_ 子树保亮)
    const setDim = (mat: THREE.MeshPhongMaterial, dim: boolean) => {
      if (dim) {
        if (mat.userData.origOpacity === undefined) {
          mat.userData.origOpacity = mat.opacity;
          mat.userData.origTransparent = mat.transparent;
          mat.userData.origDepthWrite = mat.depthWrite;
        }
        mat.transparent = true; mat.opacity = 0.22; mat.depthWrite = false;
      } else if (mat.userData.origOpacity !== undefined) {
        mat.opacity = mat.userData.origOpacity as number;
        mat.transparent = mat.userData.origTransparent as boolean;
        mat.depthWrite = mat.userData.origDepthWrite as boolean;
      }
    };
    slots.forEach((s, prefix) => {
      let mode: Slot["dimMode"] = "none";
      if (focusMode === "ghost" && visualSelected) {
        if (prefix !== visualSelected) mode = "all";
        else if (eeSubtree) mode = "body";
      }
      if (mode === s.dimMode) return;
      s.dimMode = mode;
      forEachOwnedMesh(s, (m) => {
        if (m.isMesh && !Array.isArray(m.material)) {
          const mat = m.material as THREE.MeshPhongMaterial;
          const dim = mode === "all" || (mode === "body" && !inEeSubtree(m, s.group));
          setDim(mat, dim);
        }
      });
    });

    // 选中高亮(emissive):整机 / 仅 ee_ 子树(被绑 EE 选中时只亮爪)
    slots.forEach((s, prefix) => {
      let mode: Slot["hlMode"] = "off";
      if (prefix === visualSelected) mode = eeSubtree ? "ee" : "full";
      if (mode === s.hlMode) return;
      s.hlMode = mode;
      forEachOwnedMesh(s, (m) => {
        if (m.isMesh && !Array.isArray(m.material)) {
          const mat = m.material as THREE.MeshPhongMaterial;
          const lit = mode === "full" || (mode === "ee" && inEeSubtree(m, s.group));
          if (mat.emissive) mat.emissive.set(lit ? HIGHLIGHT : 0x000000);
        }
      });
    });
  }

  const retry = (prefix: string) => {
    const slot = slotsRef.current.get(prefix);
    const robot = propsRef.current.robots.find((candidate) => candidate.prefix === prefix);
    if (slot && robot) void loadUrdfInto(slot, prefix, robot.kind_name, true);
  };

  return (
    <div style={{ position: "relative", width: "100%", height }}>
      <div ref={mountRef} style={{ width: "100%", height, borderRadius: 8, overflow: "hidden" }} />
      {loadIssues.length > 0 && (
        <div style={{
          position: "absolute",
          top: 8,
          left: 8,
          maxWidth: "min(620px, calc(100% - 16px))",
          maxHeight: Math.max(100, height - 16),
          overflow: "auto",
          display: "grid",
          gap: 6,
          pointerEvents: "none",
          zIndex: 4,
        }}>
          {loadIssues.map((issue) => (
            <div key={issue.prefix} style={{
              padding: "8px 10px",
              border: "1px solid #a85d5d",
              borderRadius: 6,
              color: "#ffe5e5",
              background: "rgba(73, 26, 30, 0.94)",
              boxShadow: "0 2px 8px rgba(0,0,0,.35)",
              fontSize: 12,
              lineHeight: 1.45,
              pointerEvents: "auto",
            }}>
              <div style={{ fontWeight: 700 }}>{issue.prefix}：3D 模型加载失败</div>
              <div>{issue.keepingOldModel ? "继续显示上一版可用模型。" : "已保留占位模型，不会显示透明骨架。"}</div>
              <pre style={{ margin: "5px 0", whiteSpace: "pre-wrap", overflowWrap: "anywhere", font: "inherit" }}>{issue.message}</pre>
              <button type="button" onClick={() => retry(issue.prefix)} disabled={issue.retrying}>
                {issue.retrying ? `自动重试中（失败 ${issue.failureCount} 次）` : "重试"}
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
