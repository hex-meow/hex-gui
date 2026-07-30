# hexmeow-stm32-can-dfu

Reusable host-side safety gates for the hexmeow STM32 CANopen DFU backend.

The API is intentionally fail-closed:

- discovery reads only the complete standard `0x1018:00..=04` record;
- every scalar has its exact CANopen width (`U8` for subcount, `U32` for identity);
- proprietary `0x2102` is read only after an exact enabled local
  `(vendor_id, product_code)` profile match;
- disabled, unknown, malformed, or unprovisioned targets cause no proprietary
  access and no SDO download;
- hardware versions, MCU names, and firmware IDs are finite exact sets—there
  is no wildcard, force, or mismatch override;
- every firmware ID lists one or more exact application `0x1008` names, and
  post-START confirmation requires both an allowed name and the expected
  software revision;
- `.meowpkg` input is parsed from bytes with archive/member/count limits before
  allocating member buffers.

Two explicit artifact policies are available:

- `UnprotectedV1` accepts only baseline plaintext-v1 packages;
- `EncryptedV2` accepts only signed + AES-256-GCM-record v2 packages and binds
  the product profile to a raw 64-byte P-256 public key (`x || y`).

For encrypted v2, each product profile explicitly pins its signing-key ID,
encryption-key ID, and security epoch. `PreparedUpgrade::bind` requires exact
header matches, checks `manifest.pubkey_fingerprint` against the profile key,
and verifies the canonical low-S ECDSA-P256 prehash signature over the
container header. Signed-only v2 is deliberately refused.

The host never receives an AES key and never decrypts `image.bin`. Archive
SHA-256/CRC protects local/download transport before mutation; the signed
header authenticates product/hardware/firmware metadata and the plaintext
digest. Ciphertext authenticity is enforced at the final boundary by the
Bootloader, which verifies each AES-GCM record before programming it. A
corrupted opaque record can therefore fail the update and leave the device in
the recoverable Bootloader, but cannot make the host disclose plaintext.

`PreparedUpgrade` is cloneable and may be retained while the GUI closes the CAN
adapter. `revalidate_prepared` must run on the freshly opened classic-CAN bus;
it compares the complete identity and hardware value with discovery before it
returns the non-constructible `ReadyToFlash` mutation capability.

`flash` consumes that capability. Before its first write it repeats the full
authorization on the exact transport passed to `flash`; this prevents a fresh
token created on adapter A from ever sending STOP to an unknown node on adapter
B. No caller callback runs between this final same-transport check and STOP.
Its write order is:

1. reject stale authorization or cancellation;
2. repeat exact identity/hardware authorization on the mutation transport;
3. issue the idempotent STOP/claim;
4. require the exact profile-specific Bootloader name;
5. re-read the full identity and bind the transition by stable
   vendor/product/serial plus exact hardware (software revision may change);
6. re-claim the confirmed Bootloader;
7. write and, after an ambiguous result, read back the container header;
8. CLEAR/arm the download;
9. stream v1 in aligned chunks, or encrypted v2 as exact
   `ciphertext || 16-byte tag` records with no extra tail padding; resolve
   every ambiguous result through the device's authoritative byte counter
   before retrying;
10. require the final byte counter, START, then confirm the same physical board
   as an explicitly allowed application name with the header's expected
   software revision.

Explicit SDO server aborts are definitive rejections and are never treated as
lost acknowledgements. Only timeout/I/O-style ambiguous results may use
readback or authoritative-offset recovery.

The engine has no terminal output or progress-bar dependency. Callers receive
typed `FlashEvent` progress and use a cloneable `CancellationToken`.
Cancellation is cooperative between SDO operations and remains safe before
START: a partial/cancelled transfer stays recoverable in the Bootloader. Once
START has been sent, the engine completes the bounded application confirmation
instead of returning an ambiguous cancellation result.

No production product mapping is built in. Applications must provide a
`TargetRegistry`; targets whose MCU/hardware/firmware mapping has not been
qualified should be registered with `RegisteredTarget::disabled`.
Each enabled target uses `FirmwarePolicy` to bind every allowed firmware ID to
its exact application name(s), plus an `ArtifactPolicy` appropriate to the
device's protection state.
`observe_identity` returns the full read-only snapshot for the UI, and
`TargetRegistry::classify` distinguishes enabled, known-disabled, unknown, and
sentinel identities without another bus operation.

The legacy CLI still uses its original engine and can be migrated to this API
in a later compatibility change.
