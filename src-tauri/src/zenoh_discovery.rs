//! Shared validation for robot-level Zenoh discovery replies.
//!
//! Robot descriptions live at `hexmeow/<controller_id>/<robot_index>/description`.
//! Device descriptions add another segment (`.../base/description`, `.../arm/description`,
//! and so on), so a recursive `**/description` selector is not type-safe: protobuf messages
//! with overlapping field numbers can decode as a plausible but bogus `RobotDescription`.

pub(crate) const ROBOT_DESCRIPTION_SELECTOR: &str = "hexmeow/*/*/description";

/// Return the robot prefix only when both the concrete reply key and decoded identity match
/// the robot-level discovery contract.
pub(crate) fn robot_prefix_from_description_reply<'a>(
    key: &'a str,
    robot_index: &str,
) -> Option<&'a str> {
    let prefix = key.strip_suffix("/description")?;
    let mut parts = prefix.split('/');
    let namespace = parts.next()?;
    let controller_id = parts.next()?;
    let key_robot_index = parts.next()?;

    if parts.next().is_some()
        || namespace != "hexmeow"
        || controller_id.is_empty()
        || key_robot_index.is_empty()
        || key_robot_index != robot_index
    {
        return None;
    }

    Some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_robot_description_reply() {
        assert_eq!(
            robot_prefix_from_description_reply(
                "hexmeow/controller-1/base0/description",
                "base0"
            ),
            Some("hexmeow/controller-1/base0")
        );
    }

    #[test]
    fn rejects_device_description_and_identity_mismatch() {
        assert_eq!(
            robot_prefix_from_description_reply(
                "hexmeow/controller-1/base0/base/description",
                "diff2"
            ),
            None
        );
        assert_eq!(
            robot_prefix_from_description_reply(
                "hexmeow/controller-1/base0/description",
                "other"
            ),
            None
        );
    }
}
