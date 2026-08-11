import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

async function source(relativePath: string): Promise<string> {
  return readFile(path.join(repoRoot, relativePath), "utf8");
}

test("CAN DFU prepared contract exposes token-bound manual risk and factory backup state", async () => {
  const api = await source("src/dfuApi.ts");

  assert.match(api, /manual_risk_required:\s*boolean/);
  assert.match(api, /manual_risk_acknowledged:\s*boolean/);
  assert.match(api, /factory_backup_required:\s*boolean/);
  assert.match(
    api,
    /acknowledgeManual:\s*\(token:\s*string\)[\s\S]*?stm32_can_dfu_acknowledge_manual[\s\S]*?\{ token \}/,
  );
});

test("every enabled CAN backend tries its identity-bound online release first", async () => {
  const ui = await source("src/components/CanDfuFlow.tsx");
  const selection = ui.slice(
    ui.indexOf("const selectDevice"),
    ui.indexOf("const chooseFile"),
  );

  assert.match(selection, /await canDfuApi\.select\(device\.node_id\)/);
  assert.match(selection, /setSelected\(device\);\s*await loadLatest\(\)/);
  assert.doesNotMatch(selection, /device\.backend\s*===\s*["']stm32_canopen/);
  assert.match(ui, /\{onlineError && \(/);
  assert.match(ui, /copy\.chooseCompatibleFileAdvanced/);
});

test("local IMG selection is risk-gated before choosing and again after parsing", async () => {
  const ui = await source("src/components/CanDfuFlow.tsx");

  assert.match(ui, /autoFocusButton:\s*["']cancel["']/);
  assert.match(ui, /canDfuApi\.acknowledgeManual\(prepared\.token\)/);
  assert.match(
    ui,
    /prepared\.manual_risk_required\s*&&\s*!prepared\.manual_risk_acknowledged/,
  );
  assert.match(ui, /prepared\.artifact_sha256\s*:\s*shortHash/);
  assert.match(ui, /prepared\.device\.vendor_id_hex/);
  assert.match(ui, /prepared\.device\.software_revision_hex/);
  assert.match(ui, /label=\{copy\.fileNameLabel\}[\s\S]*?\{fileName\}/);
  assert.match(ui, /copy\.imgHeader/);
  assert.doesNotMatch(ui, /MAX_FRONTEND_FILE_SIZE/);
});

test("IMG warnings state the local family-verification limit in both languages", async () => {
  const ui = await source("src/components/CanDfuFlow.tsx");

  assert.match(ui, /IMG 头不认证 CiA402 \/ Meow 软件族/);
  assert.match(ui, /IMG header does not authenticate the CiA402 \/ Meow software family/);
  assert.match(ui, /0x4001 出厂校准数据/);
  assert.match(ui, /0x4001 factory-calibration backup/);
});

test("online revision policy refuses downgrade while local IMG stays unproven", async () => {
  const api = await source("src/dfuApi.ts");
  const ui = await source("src/components/CanDfuFlow.tsx");

  assert.doesNotMatch(api, /version_warning:[^;]*["']downgrade["']/);
  assert.match(
    ui,
    /目标 revision 低于当前 0x1018:03 时，会在下载 IMG 前拒绝/,
  );
  assert.match(
    ui,
    /signed release target revision below the current 0x1018:03 is refused before the IMG download/,
  );
  assert.match(ui, /等版本允许重刷/);
  assert.match(ui, /equal revision may be reflashed/);
  assert.match(ui, /无法仅凭本地 IMG 证明它属于你想要的固件族/);
  assert.match(ui, /prepared\.device\.software_revision_hex/);
  assert.match(ui, /prepared\.expected_postflash_revision_hex/);
  assert.match(ui, /已验签目标 0x1018:03 revision/);
});

test("0x4001 progress and outcome contracts distinguish preservation from recovery", async () => {
  const api = await source("src/dfuApi.ts");

  assert.match(api, /["']backing_up_factory_data["']/);
  assert.match(api, /["']checking_factory_data["']/);
  assert.match(api, /["']verify_acked_factory_data_preserved["']/);
  assert.match(api, /["']verify_acked_factory_data_recovery_required["']/);
  assert.match(api, /factory_backup_path:\s*string \| null/);
  assert.match(api, /factory_backup_sha256:\s*string \| null/);
  assert.match(
    api,
    /factory_data_status:[\s\S]*?["']preserved["'][\s\S]*?["']recovery_required["'][\s\S]*?["']startup_unconfirmed["']/,
  );
});

test("0x4001 recovery is rendered as an error and exposes a copyable backup path", async () => {
  const ui = await source("src/components/CanDfuFlow.tsx");
  const outcomes = ui.slice(
    ui.indexOf("function OutcomeAlert"),
    ui.indexOf("function shortHash"),
  );

  assert.match(
    outcomes,
    /verify_acked_factory_data_recovery_required[\s\S]*?type=["']error["']/,
  );
  assert.match(
    outcomes,
    /verify_acked_factory_data_preserved[\s\S]*?type=["']success["']/,
  );
  assert.match(
    outcomes,
    /copyable=\{\{ text: outcome\.factory_backup_path \}\}/,
  );
  assert.match(outcomes, /verify_acked_startup_unconfirmed[\s\S]*?FactoryBackupReference/);
  assert.match(ui, /不能视为普通升级成功/);
  assert.match(ui, /this is not a normal update success/);
});
