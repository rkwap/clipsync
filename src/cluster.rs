//! A cluster is the set of devices that share copies. Devices are known by
//! their public keys, not their IP addresses, so a cluster survives network
//! changes. Any member can add or remove devices.
//!
//! Members exchange their lists on every connection and merge them, so a
//! change made on one device reaches the rest as soon as they talk. Removing
//! a key is permanent (a removed device that rejoins gets a new key), which
//! makes merging a plain union of members and removals: no ordering issues,
//! and a removal can never be undone by a stale copy of the list.

use std::collections::BTreeMap;
use std::io;

use crate::store::{self, DeviceId, hex, unhex32};

#[derive(Clone, PartialEq, Debug)]
pub struct Member {
    pub name: String,
    /// When it was added (ms since the Unix epoch).
    pub added: u64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Cluster {
    pub id: [u8; 32],
    pub members: BTreeMap<DeviceId, Member>,
    /// Removed devices and when they were removed.
    pub removed: BTreeMap<DeviceId, u64>,
}

pub enum Membership {
    Member,
    Removed,
    Unknown,
}

impl Cluster {
    pub fn new(founder: DeviceId, name: &str) -> io::Result<Cluster> {
        let mut c = Cluster { id: store::random()?, members: BTreeMap::new(), removed: BTreeMap::new() };
        c.add(founder, name);
        Ok(c)
    }

    pub fn membership(&self, id: &DeviceId) -> Membership {
        if self.removed.contains_key(id) {
            Membership::Removed
        } else if self.members.contains_key(id) {
            Membership::Member
        } else {
            Membership::Unknown
        }
    }

    pub fn is_member(&self, id: &DeviceId) -> bool {
        matches!(self.membership(id), Membership::Member)
    }

    /// Current members, excluding removed ones.
    pub fn active(&self) -> impl Iterator<Item = (&DeviceId, &Member)> {
        self.members.iter().filter(|(id, _)| !self.removed.contains_key(*id))
    }

    pub fn name_of(&self, id: &DeviceId) -> String {
        self.members.get(id).map_or_else(|| hex(&id[..4]), |m| m.name.clone())
    }

    pub fn add(&mut self, id: DeviceId, name: &str) {
        let name = name.replace(['\n', '\r'], " ");
        self.members.insert(id, Member { name, added: crate::hub::now_ms() });
    }

    pub fn remove(&mut self, id: DeviceId) {
        self.removed.entry(id).or_insert_with(crate::hub::now_ms);
    }

    /// Merge another copy of this cluster's list. Returns whether we learned anything.
    pub fn merge(&mut self, other: &Cluster) -> bool {
        if other.id != self.id {
            return false;
        }
        let before = self.clone();
        for (id, m) in &other.members {
            match self.members.get(id) {
                Some(mine) if mine.added >= m.added => {}
                _ => {
                    self.members.insert(*id, m.clone());
                }
            }
        }
        for (id, at) in &other.removed {
            self.removed.entry(*id).or_insert(*at);
        }
        *self != before
    }

    /// Published in mDNS so members can spot each other among everything else
    /// on the network. Derived from the cluster id, so it reveals nothing else.
    pub fn tag(&self) -> String {
        hex(&blake3::keyed_hash(&self.id, b"clipsync mdns tag").as_bytes()[..8])
    }

    pub fn to_text(&self) -> String {
        let mut s = format!("cluster {}\n", hex(&self.id));
        for (id, m) in &self.members {
            s += &format!("member {} {} {}\n", hex(id), m.added, m.name);
        }
        for (id, at) in &self.removed {
            s += &format!("removed {} {at}\n", hex(id));
        }
        s
    }

    pub fn parse(s: &str) -> io::Result<Cluster> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed cluster list");
        let mut c = Cluster { id: [0; 32], members: BTreeMap::new(), removed: BTreeMap::new() };
        let mut have_id = false;
        for line in s.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let mut f = line.splitn(4, ' ');
            match (f.next(), f.next(), f.next(), f.next()) {
                (Some("cluster"), Some(id), None, None) => {
                    c.id = unhex32(id).ok_or_else(bad)?;
                    have_id = true;
                }
                (Some("member"), Some(id), Some(added), name) => {
                    let m = Member { name: name.unwrap_or("").to_string(), added: added.parse().map_err(|_| bad())? };
                    c.members.insert(unhex32(id).ok_or_else(bad)?, m);
                }
                (Some("removed"), Some(id), Some(at), None) => {
                    c.removed.insert(unhex32(id).ok_or_else(bad)?, at.parse().map_err(|_| bad())?);
                }
                _ => return Err(bad()),
            }
        }
        if !have_id {
            return Err(bad());
        }
        Ok(c)
    }
}

pub fn load() -> io::Result<Option<Cluster>> {
    store::read("cluster")?.map(|s| Cluster::parse(&s)).transpose()
}

/// Change the cluster file while holding its lock, so the daemon and commands
/// never overwrite each other's changes.
pub fn update<T>(f: impl FnOnce(&mut Option<Cluster>) -> io::Result<T>) -> io::Result<T> {
    let _lock = store::lock("cluster.lock")?;
    let mut cluster = load()?;
    let before = cluster.clone();
    let result = f(&mut cluster)?;
    if cluster != before {
        match &cluster {
            Some(c) => store::write("cluster", &format!("# managed by clipsync; don't edit\n{}", c.to_text()))?,
            None => store::delete("cluster")?,
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster() -> Cluster {
        let mut c = Cluster::new([1; 32], "laptop").unwrap();
        c.add([2; 32], "desktop with spaces");
        c
    }

    #[test]
    fn text_round_trips() {
        let mut c = cluster();
        c.remove([2; 32]);
        assert_eq!(Cluster::parse(&c.to_text()).unwrap(), c);
    }

    #[test]
    fn rejects_malformed_lists() {
        assert!(Cluster::parse("member abc 1 x\n").is_err());
        assert!(Cluster::parse("").is_err()); // no cluster id
        let ok = cluster().to_text();
        assert!(Cluster::parse(&format!("{ok}bogus line\n")).is_err());
    }

    #[test]
    fn membership() {
        let mut c = cluster();
        assert!(c.is_member(&[1; 32]) && c.is_member(&[2; 32]));
        assert!(matches!(c.membership(&[3; 32]), Membership::Unknown));
        c.remove([2; 32]);
        assert!(matches!(c.membership(&[2; 32]), Membership::Removed));
        assert_eq!(c.active().count(), 1);
    }

    #[test]
    fn merge_is_a_union_and_removal_sticks() {
        let mut a = cluster();
        let mut b = a.clone();
        a.add([3; 32], "phone");
        b.remove([2; 32]);
        // A stale copy that still lists device 2 as a member can't undo the removal.
        let stale = cluster();
        assert!(a.merge(&b));
        assert!(!a.merge(&stale));
        assert!(a.is_member(&[3; 32]) && !a.is_member(&[2; 32]));
        assert!(b.merge(&a));
        assert_eq!(a, b);
        assert!(!a.merge(&b), "merging again changes nothing");
    }

    #[test]
    fn merge_ignores_other_clusters() {
        let mut a = cluster();
        let other = Cluster::new([7; 32], "stranger").unwrap();
        assert!(!a.merge(&other));
        assert!(!a.is_member(&[7; 32]));
    }

    #[test]
    fn newer_name_wins() {
        let mut a = cluster();
        let mut b = a.clone();
        b.members.get_mut(&[2; 32]).unwrap().added += 1;
        b.members.get_mut(&[2; 32]).unwrap().name = "renamed".into();
        a.merge(&b);
        assert_eq!(a.name_of(&[2; 32]), "renamed");
    }

    #[test]
    fn tag_depends_only_on_the_cluster() {
        let a = cluster();
        let mut b = a.clone();
        b.add([9; 32], "x");
        assert_eq!(a.tag(), b.tag());
        assert_ne!(a.tag(), Cluster::new([1; 32], "laptop").unwrap().tag());
    }
}
