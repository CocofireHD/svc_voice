//! Group state and the `voicechat:*` group payloads (SPEC §8.5–§8.10).
//!
//! Everything here is pure: bytes in, bytes out, plus the list the client keeps
//! so a name typed in chat can be turned into the id the server expects.

use super::wire::{Reader, Uuid, WireError, Writer};

/// `MAX_GROUP_NAME_LENGTH` (SPEC §8.8) — names and passwords are both capped.
pub const MAX_NAME: usize = 24;

/// `$8.5` `type`: what the server does with a group's audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    /// Audio only to the group; nobody nearby hears it.
    Normal,
    /// Audio to the group *and* to nearby players.
    Open,
    /// Audio only to the group, and the receiver hears only its own group.
    Isolated,
}

impl GroupKind {
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::Open,
            2 => Self::Isolated,
            _ => Self::Normal,
        }
    }

    pub fn as_i16(self) -> i16 {
        match self {
            Self::Normal => 0,
            Self::Open => 1,
            Self::Isolated => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Open => "open",
            Self::Isolated => "isolated",
        }
    }
}

/// `voicechat:add_group` (S→C), SPEC §8.5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub id: Uuid,
    pub name: String,
    pub has_password: bool,
    pub persistent: bool,
    pub hidden: bool,
    pub kind: GroupKind,
}

impl GroupInfo {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let id = r.read_uuid()?;
        let name = r.read_string(512)?;
        let has_password = r.read_bool()?;
        let persistent = r.read_bool()?;
        let hidden = r.read_bool()?;
        let kind = GroupKind::from_i16(r.read_i16()?);
        r.ensure_end()?;
        Ok(Self {
            id,
            name,
            has_password,
            persistent,
            hidden,
            kind,
        })
    }
}

/// `voicechat:joined_group` (S→C), SPEC §8.10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Joined {
    pub group: Option<Uuid>,
    pub wrong_password: bool,
}

impl Joined {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let has_group = r.read_bool()?;
        let group = if has_group { Some(r.read_uuid()?) } else { None };
        let wrong_password = r.read_bool()?;
        r.ensure_end()?;
        Ok(Self {
            group,
            wrong_password,
        })
    }
}

/// `voicechat:remove_group` (S→C), SPEC §8.6: the id alone.
pub fn parse_remove_group(bytes: &[u8]) -> Result<Uuid, WireError> {
    let mut r = Reader::new(bytes);
    let id = r.read_uuid()?;
    r.ensure_end()?;
    Ok(id)
}

/// `voicechat:create_group` (C→S), SPEC §8.8.
pub fn create_group(name: &str, password: Option<&str>, kind: GroupKind) -> Result<Vec<u8>, WireError> {
    // The server's regex wants a non-space first character and no control
    // characters; refusing here means the player gets an answer instead of a
    // silent drop.
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(WireError::StringCharLimit {
            len: name.chars().count(),
            max: MAX_NAME,
        });
    }
    if name.starts_with(char::is_whitespace) || name.chars().any(char::is_control) {
        // The server's regex rejects these; so do we, before the round trip.
        return Err(WireError::InvalidUtf8);
    }
    let mut w = Writer::new();
    w.write_string(name, MAX_NAME)?;
    w.write_bool(password.is_some());
    if let Some(p) = password {
        w.write_string(p, MAX_NAME)?;
    }
    w.write_i16(kind.as_i16());
    Ok(w.into_inner())
}

/// `voicechat:set_group` (C→S), SPEC §8.7: join by id, password optional.
pub fn set_group(id: Uuid, password: Option<&str>) -> Result<Vec<u8>, WireError> {
    let mut w = Writer::new();
    w.write_uuid(id);
    w.write_bool(password.is_some());
    if let Some(p) = password {
        w.write_string(p, MAX_NAME)?;
    }
    Ok(w.into_inner())
}

/// `voicechat:leave_group` (C→S), SPEC §8.9: empty payload.
pub fn leave_group() -> Vec<u8> {
    Vec::new()
}

/// The client's view of the server's groups plus the one it is in.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Groups {
    pub list: Vec<GroupInfo>,
    pub current: Option<Uuid>,
}

impl Groups {
    /// `voicechat:add_group` replaces by id, because the server re-sends the
    /// whole list after a compatibility check (§8.5, §6.2).
    pub fn insert(&mut self, info: GroupInfo) {
        match self.list.iter_mut().find(|g| g.id == info.id) {
            Some(slot) => *slot = info,
            None => self.list.push(info),
        }
    }

    pub fn remove(&mut self, id: Uuid) {
        self.list.retain(|g| g.id != id);
        if self.current == Some(id) {
            self.current = None;
        }
    }

    pub fn current_info(&self) -> Option<&GroupInfo> {
        let id = self.current?;
        self.list.iter().find(|g| g.id == id)
    }

    /// Names are matched case-insensitively: a player types what the chat line
    /// showed, not byte-identical casing.
    pub fn by_name(&self, name: &str) -> Option<&GroupInfo> {
        self.list.iter().find(|g| g.name.eq_ignore_ascii_case(name))
    }

    /// What `/vc list` prints, and what the log shows when a group changes.
    pub fn summary(&self) -> String {
        if self.list.is_empty() {
            return "no groups".to_string();
        }
        let mut out = String::new();
        for g in &self.list {
            if !out.is_empty() {
                out.push_str(", ");
            }
            out.push_str(&g.name);
            if g.has_password {
                out.push('*');
            }
            if g.kind != GroupKind::Normal {
                out.push('/');
                out.push_str(g.kind.name());
            }
            if self.current == Some(g.id) {
                out.push_str(" (joined)");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: i64, name: &str) -> GroupInfo {
        GroupInfo {
            id: Uuid::from_parts(id, 7),
            name: name.to_string(),
            has_password: false,
            persistent: false,
            hidden: false,
            kind: GroupKind::Normal,
        }
    }

    #[test]
    fn add_group_round_trips_through_the_payload() {
        let mut w = Writer::new();
        w.write_uuid(Uuid::from_parts(11, 22));
        w.write_string("Team", 512).unwrap();
        w.write_bool(true);
        w.write_bool(false);
        w.write_bool(true);
        w.write_i16(2);
        let info = GroupInfo::parse(w.as_slice()).unwrap();
        assert_eq!(info.id, Uuid::from_parts(11, 22));
        assert_eq!(info.name, "Team");
        assert!(info.has_password);
        assert!(!info.persistent);
        assert!(info.hidden);
        assert_eq!(info.kind, GroupKind::Isolated);
    }

    #[test]
    fn create_group_carries_name_password_and_type() {
        let bytes = create_group("Squad", Some("pw"), GroupKind::Open).unwrap();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_string(MAX_NAME).unwrap(), "Squad");
        assert!(r.read_bool().unwrap());
        assert_eq!(r.read_string(MAX_NAME).unwrap(), "pw");
        assert_eq!(r.read_i16().unwrap(), 1);
        r.ensure_end().unwrap();
    }

    #[test]
    fn create_group_without_password_says_so() {
        let bytes = create_group("Squad", None, GroupKind::Normal).unwrap();
        let mut r = Reader::new(&bytes);
        let _ = r.read_string(MAX_NAME).unwrap();
        assert!(!r.read_bool().unwrap());
        // Straight to the type: no password string in between.
        assert_eq!(r.read_i16().unwrap(), 0);
        r.ensure_end().unwrap();
    }

    #[test]
    fn a_bad_name_is_refused_before_it_reaches_the_server() {
        assert!(create_group("", None, GroupKind::Normal).is_err());
        assert!(create_group(" leading", None, GroupKind::Normal).is_err());
        assert!(create_group("tab\there", None, GroupKind::Normal).is_err());
        assert!(create_group(&"x".repeat(25), None, GroupKind::Normal).is_err());
        assert!(create_group(&"x".repeat(24), None, GroupKind::Normal).is_ok());
    }

    #[test]
    fn joining_needs_the_id_and_sometimes_a_password() {
        let bytes = set_group(Uuid::from_parts(3, 4), Some("pw")).unwrap();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_uuid().unwrap(), Uuid::from_parts(3, 4));
        assert!(r.read_bool().unwrap());
        assert_eq!(r.read_string(MAX_NAME).unwrap(), "pw");

        let bytes = set_group(Uuid::from_parts(3, 4), None).unwrap();
        assert_eq!(bytes.len(), 17, "uuid and a false flag");
        assert!(leave_group().is_empty());
    }

    #[test]
    fn joined_group_reports_leaving_and_a_wrong_password() {
        let mut w = Writer::new();
        w.write_bool(true);
        w.write_uuid(Uuid::from_parts(9, 9));
        w.write_bool(false);
        let j = Joined::parse(w.as_slice()).unwrap();
        assert_eq!(j.group, Some(Uuid::from_parts(9, 9)));
        assert!(!j.wrong_password);

        let mut w = Writer::new();
        w.write_bool(false);
        w.write_bool(true);
        let j = Joined::parse(w.as_slice()).unwrap();
        assert_eq!(j.group, None);
        assert!(j.wrong_password);
    }

    #[test]
    fn the_list_replaces_by_id_and_forgets_a_removed_group() {
        let mut groups = Groups::default();
        let a = group(1, "Alpha");
        groups.insert(a.clone());
        groups.insert(group(2, "Beta"));
        groups.current = Some(a.id);
        // Re-sent after a compatibility check: same id, new name.
        let mut renamed = a.clone();
        renamed.name = "Alpha2".into();
        groups.insert(renamed);
        assert_eq!(groups.list.len(), 2);
        assert_eq!(groups.by_name("alpha2").map(|g| g.id), Some(a.id));

        groups.remove(a.id);
        assert_eq!(groups.list.len(), 1);
        assert_eq!(groups.current, None, "leaving drops the current group");
    }

    #[test]
    fn the_summary_marks_a_password_a_type_and_the_current_group() {
        let mut groups = Groups::default();
        let mut a = group(1, "Alpha");
        a.has_password = true;
        a.kind = GroupKind::Open;
        groups.insert(a.clone());
        groups.current = Some(a.id);
        assert_eq!(groups.summary(), "Alpha*/open (joined)");
        assert_eq!(Groups::default().summary(), "no groups");
    }
}
