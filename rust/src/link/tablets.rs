//! The paired tablets, the one in use (the only one that gets the desktop
//! and the PDFs to sign, whose camera, microphone and speakers are the
//! PC's), and where its app is: announced, or where it was last (tablets.py).

use super::discovery::Source;
use super::link::Target;
use super::trust::{Paired, MAX_NAME};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tablet {
    pub fingerprint: String,  // of its app's certificate
    pub name: String,
}

impl Tablet {
    /// What its app announces itself as.
    pub fn app_id(&self) -> &str {
        &self.fingerprint[..16.min(self.fingerprint.len())]
    }
}

#[derive(Clone)]
pub struct Tablets {
    pub paired: Paired,
    pub discovery: Option<Arc<dyn Source>>,
    in_use: Option<String>,  // the settings' tablet
}

impl Tablets {
    pub fn new(in_use: Option<String>, discovery: Option<Arc<dyn Source>>, paired: Paired) -> Tablets {
        Tablets { paired, discovery, in_use }
    }

    /// The paired tablets, by name.
    pub fn all(&self) -> Vec<Tablet> {
        let mut tablets: Vec<Tablet> = self.paired.all().into_iter()
            .map(|(fingerprint, name)| Tablet { fingerprint, name: name.chars().take(MAX_NAME).collect() })
            .collect();
        tablets.sort_by(|a, b| (a.name.to_lowercase(), &a.fingerprint).cmp(&(b.name.to_lowercase(), &b.fingerprint)));
        tablets
    }

    /// The tablet in use: the chosen one, else the first.
    pub fn active(&self) -> Option<Tablet> {
        let tablets = self.all();
        tablets.iter().find(|t| Some(&t.fingerprint) == self.in_use.as_ref()).cloned()
            .or_else(|| tablets.into_iter().next())
    }

    /// Where its app is announced; the first network address is remembered.
    pub fn addresses(&self, tablet: &Tablet) -> Vec<Target> {
        let found = self.discovery.as_ref().map(|d| d.addresses(tablet.app_id())).unwrap_or_default();
        if let Some(Target::Tcp(host, port)) = found.first() {
            let _ = self.paired.learn_app_at(&tablet.fingerprint, &format!("{host}:{port}"));
        }
        found
    }

    /// Where to reach its app: where it is announced, else where it was last
    /// (mDNS can miss a tablet asleep a while; the TLS link proves it is the tablet).
    pub fn targets(&self, tablet: &Tablet) -> Vec<Target> {
        let found = self.addresses(tablet);
        if !found.is_empty() {
            return found;
        }
        if let Some(discovery) = &self.discovery {
            discovery.refresh();
        }
        let entries = self.paired.entries();
        let Some(at) = entries.get(&tablet.fingerprint).and_then(|e| e.app_at.as_deref()) else { return vec![] };
        match at.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
                Ok(port) if port > 0 => vec![Target::Tcp(host.to_owned(), port)],
                _ => vec![],
            },
            _ => vec![],
        }
    }
}
