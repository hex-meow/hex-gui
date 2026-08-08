# TODO

## Dependency security maintenance

The 2026-08-08 `cargo-audit 0.22.2` baseline reports seven RustSec
vulnerabilities and 23 allowed warnings. These findings are outside the product
authenticity HTTP code path, but they must be resolved or explicitly justified
before making the audit a required CI check.

- [ ] Update Zenoh or disable unused default features to remove
      `crossbeam-epoch 0.9.18` (RUSTSEC-2026-0204), `lz4_flex 0.10.0`
      (RUSTSEC-2026-0041), and `rsa 0.9.10` (RUSTSEC-2023-0071). Confirm the
      application does not need Zenoh compression or `auth_pubkey` before
      disabling either feature.
- [ ] Update the `urdf-rs` / `hex-arm-dynamics` dependency chain to a patched
      `quick-xml` release for RUSTSEC-2026-0194 and RUSTSEC-2026-0195, and keep
      explicit size/complexity limits on URDF input received over Zenoh.
- [ ] Update the Tauri / `plist` dependency chain to a patched `quick-xml`
      release for the same two advisories.
- [ ] Review the 23 allowed warnings, upgrade direct dependencies where
      practical, and record narrowly scoped exceptions for dependencies that
      cannot yet be replaced.
- [ ] Add scheduled and pull-request `cargo audit` checks after the known
      findings have been cleared or explicitly policy-allowed.
