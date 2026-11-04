#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Matching,
    Send,
    Receive,
    Conflict,
}

pub fn reconcile(local: Option<&str>, remote: Option<&str>, base: Option<&str>) -> Decision {
    if local == remote {
        Decision::Matching
    } else if local.is_none() {
        Decision::Receive
    } else if remote.is_none() || remote == base {
        Decision::Send
    } else if local == base {
        Decision::Receive
    } else {
        Decision::Conflict
    }
}

pub fn fast_forward(
    current: Option<&str>,
    incoming: &str,
    base: Option<&str>,
    offered_base: Option<&str>,
) -> bool {
    current.is_none()
        || current == Some(incoming)
        || (current == offered_base && base == offered_base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_games_use_the_same_common_base_rules() {
        assert_eq!(reconcile(None, None, None), Decision::Matching);
        assert_eq!(
            reconcile(Some("same"), Some("same"), None),
            Decision::Matching
        );
        assert_eq!(
            reconcile(Some("new"), Some("base"), Some("base")),
            Decision::Send
        );
        assert_eq!(
            reconcile(Some("base"), Some("new"), Some("base")),
            Decision::Receive
        );
        assert_eq!(
            reconcile(Some("local"), Some("remote"), Some("base")),
            Decision::Conflict
        );
        assert_eq!(
            reconcile(Some("local"), Some("remote"), None),
            Decision::Conflict
        );
        assert_eq!(reconcile(Some("progress"), None, None), Decision::Send);
        assert_eq!(reconcile(None, Some("progress"), None), Decision::Receive);
    }

    #[test]
    fn automatic_receive_never_replaces_divergent_progress() {
        assert!(fast_forward(None, "incoming", None, None));
        assert!(fast_forward(
            Some("base"),
            "incoming",
            Some("base"),
            Some("base")
        ));
        assert!(!fast_forward(
            Some("local"),
            "incoming",
            Some("base"),
            Some("base")
        ));
        assert!(!fast_forward(
            Some("base"),
            "incoming",
            Some("base"),
            Some("other")
        ));
    }
}
