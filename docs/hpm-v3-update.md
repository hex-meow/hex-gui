# HPM V3 USB / Classic CAN 升级

GUI 的 USB 页面同时支持旧 gs_can USB v2 和 HPM OTA/3。CAN 已合并为[统一入口](can-update-entry.md)：默认监听心跳并核对完整 1018，选择已知 HPM 后捕获；LSS 只在显式选择型号后的故障恢复模式开放。STM32／Motor 保留各自原生后端。

## 捕获与等待

先点击等待，再给下位机上电／复位。USB 最长等待 30 秒，只接受唯一匹配的 vendor bulk 设备；连接多台 USB 目标时明确报错。CAN 等目标 `700+Node-ID` 心跳后才发只读探测，不会在无人 ACK 时预先灌入升级帧。适配器须保持正常 ACK，不能设 silent。

正常入口从发现列表选择已有地址的 HPM。故障恢复模式确认 HPM 型号后，填写已有 Node-ID；未配置时勾选 LSS，填写已知 Vendor/Product 和确认空闲的分配 ID（1..127，排除 100/101）；默认 EVK3 Product 为 `0x45564B33`，产品板应填写自己的值。可填写完整 UID 进一步约束目标。LSS 仅匹配已知 vendor/product/revision，逐位扫描 serial，Configure／Store 后等新地址 boot-up，再进行 SDO INFO／CLAIM。目前一次捕获一个目标；不承诺多节点批量升级。

成功 CLAIM 后设备在 RAM 锁存等待，**不发送周期保活，不会因工具暂停、关闭、断线或长期无报文自动启动 APP**。RELEASE 只释放接口 owner，仍保持 WAITING。工具崩溃且 owner 未释放时可以重新上电；上电重新校验 APP／journal，APP 有效且未再次捕获时正常启动。UPDATING／无效 APP 仍留在恢复入口。

GUI 持有捕获所用原生 session／USB interface／CAN lease；选择文件时无需重新捕获。升级前再次读取完整身份，文件 token 仅使用一次，取消或结束后应重新捕获。

## 设备与文件的识别

- 只有精确的旧 `GET_INFO.bl_version == 0x0100` 继续映射为 `gcan`（gs_can）；旧明文 `.bin`、受保护 `.hpmota/2`、在线 R2 profile 与 KN_DATA 流程保留。
- V3 的 `bl_version == 0x0200`／`max_format == 3` 从 INFO 读取实际 Product、UID、capabilities 和安全模式，不按版本猜测产品。
- V3 仅接收签名 `.hpmota/3` ZIP，检查成员数量／名称／解压大小、完整镜像摘要、ROM header、manifest padding、产品／硬件范围、ARV floor、P-256 签名与设备公钥指纹。支持 stored 和 deflated ZIP。
- V3 暂无对应在线发布 profile，只使用本地签名文件；加密 V3 设备／包明确拒绝，不能降级成明文。EVK 私钥不随 GUI 分发。

V3 执行 `PREPARE → WRITE → COMMIT → JUMP`；擦写前校验签名和兼容范围。WRITE 命令之间可取消，COMMIT／JUMP 等待完整应答。GUI 区分 JUMP ACK 与 APP 健康确认，当前没有通用 APP 健康通道。

## CAN 对象字典与长度

| 对象 | 类型／权限 | 含义 |
| --- | --- | --- |
| `1018:00` | u8 RO | 4 |
| `1018:01..04` | u32 RO | vendor / product / revision / UID-hash serial |
| `2110:00` | u8 RO | 2 |
| `2110:01` | DOMAIN WO | 完整 V3 envelope 请求 |
| `2110:02` | DOMAIN RO | 对应 V3 envelope 应答 |

主机请求 `600+Node-ID`、目标应答 `580+Node-ID`；LSS 使用 `7E5/7E4`。所有升级帧都是 **11 位标准 Classic CAN data frame，最多 8 字节**。不使用 CAN-FD、BRS、扩展帧或 RTR。SocketCAN 链路由系统管理，须预先配置 nominal 1 Mbit/s；物理链路可以容纳其他设备的 FD 报文，但它们不会进入本升级协议。gs_usb 按 Classic 1M 打开。

复用 `canopen-sdo` 的标准 segmented SDO，每个 data segment 最多承载 7 字节；SDO 每步有 timeout，整个 envelope 共用截止时间。ACK 丢失时仅重试完整 envelope／相同 request ID，设备缓存避免重复擦除。上传应答在分配 buffer 前检查 size≤136。目标掉电后的硬件 TX 队列取消仍取决于适配器，不能由软件 timeout 推断已取消硬件重发。

未来加密预留的 128 字节 opaque wrap／KN 数据走同一 DOMAIN：128 字节本身需要 19 个 SDO 数据 segment；完整 PREPARE（descriptor96 + signature64 + wrap128 + session12 + envelope8）共 308 字节，需要 44 段，另加 initiate／ACK。**V3 加密／wrap／EXIP 路径尚未实现**，不复用旧版独立 WRITE_KN_DATA opcode；预留承载不代表已冻结全部加密格式。

## 验证与平台边界

Linux EVK 实测记录见 bootloader 的 `experiments/validation-v3-latched-gui-2026-10-26.md`。GUI 使用跨平台 nusb、can-transport／canopen-sdo，未进行 macOS／Windows 真机验收；Windows 首次 WinUSB 绑定、macOS 插拔及不同 CAN 适配器仍需原机验证。

Rust 核心回归：`cargo test --offline --manifest-path src-tauri/Cargo.toml -p hpm-usb-dfu`。主程序回归：`cargo test --offline --manifest-path src-tauri/Cargo.toml --lib`。前端：`npm test`、`npx tsc --noEmit`、`npm run build`。

显式实板 helper：`cargo run --manifest-path src-tauri/Cargo.toml -p hpm-usb-dfu --example v3_usb -- signed.hpmota 35`，先等待再启动目标，静默 35 秒后写入签名 APP。CAN 对应 ignored test 需明确设置 `HPM_V3_LIVE_CAN`、`HPM_V3_LIVE_ARTIFACT`；它会写入 APP，不能混入默认测试。
