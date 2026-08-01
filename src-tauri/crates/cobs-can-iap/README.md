# cobs-can-iap

`cobs-can-iap` is a reusable host-side implementation of a small firmware
update protocol transported in COBS-framed messages over Classic CAN.

The crate deliberately contains no product table. Applications must provide a
finite local registry that binds an exact CANopen identity to the protocol
identity, allowed firmware IDs, address range, maximum image size, and
encryption policy. A parsed image alone can never authorize a write.

The safe API is capability-based:

```text
complete CANopen identity
  -> local registry authorization
  -> strict IMG parsing and policy binding
  -> same-bus identity revalidation
  -> Reset / Enter identity verification
  -> first destructive download command
```

Unknown, incomplete, changed, or sentinel identities fail before any
protocol-specific frame is transmitted. Once a destructive request may have
reached the device, an ambiguous response ends the session; the core never
blindly retransmits it.

The protocol core is independent of CLI and GUI frameworks. It uses the
`can-transport` traits so SocketCAN, gs_usb, and deterministic test transports
share the same state machine.
