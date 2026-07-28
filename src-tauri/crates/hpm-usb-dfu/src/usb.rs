use std::time::Duration;

use nusb::{
    transfer::{Buffer, Bulk, In, Out},
    Endpoint, Interface, MaybeFuture,
};

use crate::protocol::{
    decode_response, encode_request, parse_chip_info, status_name, DeviceInfo, CMD_ERASE,
    CMD_GET_INFO, CMD_JUMP_APP, CMD_PING, CMD_VERIFY, CMD_WRITE, CMD_WRITE_KN_DATA, MAX_PACKET,
    WRITE_DATA_MAX,
};
use crate::{BootloaderTransport, DfuError, JumpDisposition, Result, USB_PID, USB_VID};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const ERASE_TIMEOUT: Duration = Duration::from_secs(30);
const JUMP_TIMEOUT: Duration = Duration::from_secs(2);
const WARMUP_TIMEOUT: Duration = Duration::from_millis(500);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

pub(crate) struct UsbBootloader {
    _interface: Interface,
    ep_out: Endpoint<Bulk, Out>,
    ep_in: Endpoint<Bulk, In>,
    read_len: usize,
    seq: u8,
}

impl UsbBootloader {
    pub(crate) fn open_unique() -> Result<Self> {
        let devices = nusb::list_devices()
            .wait()
            .map_err(|error| DfuError::Usb(format!("enumeration failed: {error}")))?
            .filter(|device| device.vendor_id() == USB_VID && device.product_id() == USB_PID)
            .collect::<Vec<_>>();
        match devices.len() {
            0 => {
                return Err(DfuError::DeviceNotFound {
                    vid: USB_VID,
                    pid: USB_PID,
                })
            }
            1 => {}
            count => return Err(DfuError::MultipleDevices { count }),
        }

        let device = devices[0]
            .open()
            .wait()
            .map_err(|error| DfuError::Usb(format!("open failed: {error}")))?;
        let interface = device
            .detach_and_claim_interface(0)
            .wait()
            .map_err(|error| {
                DfuError::Usb(format!(
                    "claim interface 0 failed: {error}; on Linux install the 34b7:beef udev rule"
                ))
            })?;
        let ep_out = interface.endpoint::<Bulk, Out>(0x01).map_err(|error| {
            DfuError::Usb(format!("open bulk OUT endpoint 0x01 failed: {error}"))
        })?;
        let ep_in = interface.endpoint::<Bulk, In>(0x81).map_err(|error| {
            DfuError::Usb(format!("open bulk IN endpoint 0x81 failed: {error}"))
        })?;
        let max_packet_size = ep_in.max_packet_size();
        if max_packet_size == 0 {
            return Err(DfuError::Usb(
                "bulk IN endpoint reported max packet size 0".into(),
            ));
        }
        let read_len = MAX_PACKET.div_ceil(max_packet_size) * max_packet_size;
        let mut this = Self {
            _interface: interface,
            ep_out,
            ep_in,
            read_len,
            seq: 0,
        };
        this.warmup();
        Ok(this)
    }

    fn next_seq(&mut self) -> u8 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn warmup(&mut self) {
        for _ in 0..3 {
            if self.command(CMD_PING, &[], WARMUP_TIMEOUT).is_ok() {
                return;
            }
            self.drain();
        }
        // Match the proven CLI behavior: warmup absorbs first-transfer
        // flakiness when possible, while GET_INFO surfaces the real error.
    }

    fn drain(&mut self) {
        loop {
            let completion = self
                .ep_in
                .transfer_blocking(Buffer::new(self.read_len), DRAIN_TIMEOUT);
            if completion.status.is_err() {
                break;
            }
        }
    }

    fn write_packet(&mut self, packet: Vec<u8>, timeout: Duration) -> Result<()> {
        let expected = packet.len();
        let completion = self.ep_out.transfer_blocking(packet.into(), timeout);
        completion
            .status
            .map_err(|error| DfuError::Usb(format!("bulk OUT failed: {error}")))?;
        if completion.actual_len != expected {
            return Err(DfuError::Usb(format!(
                "short bulk OUT transfer: {}/{} bytes",
                completion.actual_len, expected
            )));
        }
        Ok(())
    }

    fn read_response(&mut self, cmd: u8, seq: u8, timeout: Duration) -> Result<(u16, Vec<u8>)> {
        let completion = self
            .ep_in
            .transfer_blocking(Buffer::new(self.read_len), timeout);
        completion
            .status
            .map_err(|error| DfuError::Usb(format!("bulk IN failed: {error}")))?;
        let actual_len = completion.actual_len;
        if actual_len > completion.buffer.len() {
            return Err(DfuError::Usb(
                "bulk IN completion length exceeds buffer".into(),
            ));
        }
        let response = decode_response(&completion.buffer[..actual_len], cmd, seq)?;
        Ok((response.status, response.body.to_vec()))
    }

    fn request(&mut self, cmd: u8, payload: &[u8], timeout: Duration) -> Result<(u16, Vec<u8>)> {
        let seq = self.next_seq();
        let packet = encode_request(cmd, seq, payload)?;
        self.write_packet(packet, timeout)?;
        self.read_response(cmd, seq, timeout)
    }

    fn command(&mut self, cmd: u8, payload: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        let (status, body) = self.request(cmd, payload, timeout)?;
        if status != 0 {
            return Err(DfuError::DeviceStatus {
                cmd,
                status,
                name: status_name(status),
            });
        }
        Ok(body)
    }
}

impl BootloaderTransport for UsbBootloader {
    fn get_info(&mut self) -> Result<DeviceInfo> {
        let body = self.command(CMD_GET_INFO, &[], DEFAULT_TIMEOUT)?;
        parse_chip_info(&body)
    }

    fn erase(&mut self, address: u32, size: u32) -> Result<()> {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&address.to_le_bytes());
        payload.extend_from_slice(&size.to_le_bytes());
        self.command(CMD_ERASE, &payload, ERASE_TIMEOUT)?;
        Ok(())
    }

    fn write(&mut self, address: u32, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > WRITE_DATA_MAX {
            return Err(DfuError::Protocol(format!(
                "WRITE data length {} is outside 1..={WRITE_DATA_MAX}",
                data.len()
            )));
        }
        let mut payload = Vec::with_capacity(4 + data.len());
        payload.extend_from_slice(&address.to_le_bytes());
        payload.extend_from_slice(data);
        self.command(CMD_WRITE, &payload, DEFAULT_TIMEOUT)?;
        Ok(())
    }

    fn verify(&mut self, address: u32, size: u32, expected_crc32: u32) -> Result<u32> {
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&address.to_le_bytes());
        payload.extend_from_slice(&size.to_le_bytes());
        payload.extend_from_slice(&expected_crc32.to_le_bytes());
        let (status, body) = self.request(CMD_VERIFY, &payload, DEFAULT_TIMEOUT)?;
        if body.len() != 4 {
            return Err(DfuError::Protocol(format!(
                "VERIFY response must contain exactly 4 CRC bytes, got {}",
                body.len()
            )));
        }
        let actual = u32::from_le_bytes(body.try_into().unwrap());
        if status != 0 {
            if status == 6 {
                return Err(DfuError::VerifyMismatch {
                    expected: expected_crc32,
                    actual,
                });
            }
            return Err(DfuError::DeviceStatus {
                cmd: CMD_VERIFY,
                status,
                name: status_name(status),
            });
        }
        Ok(actual)
    }

    fn write_kn_data(&mut self, blob: &[u8; 128]) -> Result<()> {
        self.command(CMD_WRITE_KN_DATA, blob, DEFAULT_TIMEOUT)?;
        Ok(())
    }

    fn jump_app(&mut self) -> Result<JumpDisposition> {
        let seq = self.next_seq();
        let packet = encode_request(CMD_JUMP_APP, seq, &[])?;
        self.write_packet(packet, DEFAULT_TIMEOUT)?;
        match self.read_response(CMD_JUMP_APP, seq, JUMP_TIMEOUT) {
            Ok((0, body)) if body.is_empty() => Ok(JumpDisposition::Acked),
            Ok((status, _)) if status != 0 => Err(DfuError::DeviceStatus {
                cmd: CMD_JUMP_APP,
                status,
                name: status_name(status),
            }),
            Ok(_) | Err(_) => Ok(JumpDisposition::OutcomeUnknown),
        }
    }
}
