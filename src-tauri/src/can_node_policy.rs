//! Network-wide CANopen Node-ID reservations: Motor-IAP TX IDs collide with LSS.
pub const RESERVED: [u8; 2] = [100, 101];
pub fn check(node: u8) -> Result<(), String> {
    if RESERVED.contains(&node) {
        return Err(format!("Node-ID {node} (0x{node:02X}) is reserved: Motor IAP 0x780+Node-ID collides with LSS 0x7E4/0x7E5. Use Device Settings to assign another ID before control or update."));
    }
    if !(1..=127).contains(&node) {
        return Err("Node-ID must be 1..127".into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservations_match_wire_collisions() {
        for id in RESERVED {
            assert_eq!(0x780 + u16::from(id), if id == 100 { 0x7e4 } else { 0x7e5 });
            assert!(check(id).is_err());
        }
        for id in [1, 31, 99, 102, 127] {
            assert!(check(id).is_ok());
        }
    }
}
