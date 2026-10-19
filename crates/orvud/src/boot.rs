//! Builds the engine from a config file and finds the replica id that goes
//! with it.
//!
//! `Dataplane` tells every client its replica id (`OpenResponse.replica_id`),
//! and a client uses that number as the target of `bounded_transfer`. If the
//! number is not the one the engine really runs under, allowance moves to a
//! replica that does not exist. Only the holder of an allowance can transfer
//! it, so that loss cannot be repaired.
//!
//! Only `Client::from_toml` builds the engine, so the sidecar behaves the same
//! as a program that embeds the engine with the same file. Only the replica id
//! and the namespace are worked out a second time here, with the same public
//! parser (`SdkConfig::parse_toml`). `Open` checks the namespace a session
//! asks for against this one, so it must come from the same file as the
//! engine.

use orvu_crdt::ReplicaId;
use orvu_sdk::{Client, Result, SdkConfig, SdkError};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The replica id and namespace that a config text declares. Both are parsed
/// here because `orvu_sdk::Client` has no way to ask for either.
pub fn identity_of(text: &str) -> Result<(ReplicaId, String)> {
    let cfg = SdkConfig::parse_toml(text)?;
    Ok((ReplicaId(cfg.replica), cfg.namespace))
}

/// The replica id a config text declares.
pub fn replica_of(text: &str) -> Result<ReplicaId> {
    Ok(identity_of(text)?.0)
}

/// Builds the engine from a TOML file, talking to other machines over QUIC.
/// Returns it with the replica id and the namespace it runs under, and the
/// claim on its storage, which is to be kept for as long as the engine runs.
/// This is `read`, `claim` and `start`. Settings whose namespace is `local`
/// are refused by `read`.
pub async fn from_toml(path: &Path) -> Result<(Arc<Client>, ReplicaId, String, Claim)> {
    let (mut settings, folder) = read(path)?;
    let claim = claim(&mut settings, &folder)?;
    let (client, replica, namespace) = start(settings, &folder, &claim).await?;
    Ok((client, replica, namespace, claim))
}

/// The hold a sidecar has on its storage folder. Keep it for as long as the
/// engine runs: dropping it lets another sidecar use the folder.
pub struct Claim {
    _lock: File,
    /// The folder to write the machine number into, and the number, once the
    /// engine has started under it.
    to_note: Option<(PathBuf, u64)>,
}

/// Takes the storage folder the settings name for this sidecar, and checks
/// that it belongs to the machine the settings say this is.
///
/// The folder is made if it is missing, and locked, so that a second sidecar
/// started on it, from the same settings file or another, is refused. With
/// no storage, everything is kept in memory and the settings file's own
/// folder is locked instead: two sidecars from one settings file would be
/// two machines with one number.
///
/// The storage folder keeps the number it was first used with, in a file
/// named `replica`, as a local sidecar's data folder does. It is written by
/// `start`, once the engine has taken the data under that number, so a start
/// that is refused writes nothing. Settings that later name another number
/// are refused. The `replica` file is never taken as leave to adopt data
/// with no note of its own number: only `adopt_data` in the settings is.
pub fn claim(settings: &mut SdkConfig, folder: &Path) -> Result<Claim> {
    let Some(store) = settings.storage.path.as_ref().map(|path| folder.join(path)) else {
        let held = if folder.as_os_str().is_empty() { Path::new(".") } else { folder };
        let lock = crate::local::lock(held, || in_use("settings folder", held))?;
        return Ok(Claim { _lock: lock, to_note: None });
    };
    let holder = match store.parent() {
        Some(holder) if !holder.as_os_str().is_empty() => holder.to_path_buf(),
        _ => PathBuf::from("."),
    };
    orvu_sdk::make_private_folder(&holder)
        .map_err(|e| SdkError::Config(format!("cannot make the storage folder {}: {e}", holder.display())))?;
    let lock = crate::local::lock(&holder, || in_use("storage folder", &holder))?;
    let path = holder.join("replica");
    let kept = match crate::local::read_number(&path)? {
        Some(kept) => kept,
        // Nothing is written yet. The number goes into the folder once the
        // engine has started under it, so a start that is refused leaves
        // nothing behind for the next one to go by.
        None => return Ok(Claim { _lock: lock, to_note: Some((holder, settings.replica)) }),
    };
    // The data's own note of its number is what counts. A folder file that
    // says otherwise is wrong, and it is said which file to put right.
    let note = crate::local::data_note(&store);
    if let Some(noted) = crate::local::read_number(&note)?.filter(|&noted| noted != kept) {
        return Err(SdkError::Config(format!(
            "{} says this storage belongs to machine {kept}, but {} says the data in it was written as \
             machine {noted}. The data's own note is the one to go by: delete {} (it is written again on \
             the next start), and keep replica = {noted} in the settings, with the certificate for it",
            path.display(),
            note.display(),
            path.display()
        )));
    }
    if kept != settings.replica {
        return Err(SdkError::Config(format!(
            "the storage folder {} belongs to machine {kept}, and the settings say replica = {}. \
             A machine keeps its number for as long as it keeps its data: put {kept} back in the \
             settings, with the certificate for it, or give this machine storage of its own",
            holder.display(),
            settings.replica
        )));
    }
    Ok(Claim { _lock: lock, to_note: None })
}

/// What a sidecar is told when another one holds `folder`.
fn in_use(what: &str, folder: &Path) -> String {
    format!(
        "another orvud is already using the {what} {}: stop it, or give this one storage of its own in its settings",
        folder.display()
    )
}

/// Reads a settings file once. Returns the settings and the folder the file
/// is in, which paths in it are taken from.
pub fn read(path: &Path) -> Result<(SdkConfig, std::path::PathBuf)> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| SdkError::Config(format!("{}: {e}", path.display())))?;
    let folder = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let settings = SdkConfig::parse_toml(&text)?;
    if settings.namespace == crate::local::LOCAL_NAMESPACE {
        return Err(SdkError::Config(format!(
            "{}: the namespace cannot be {:?}. That is the namespace of a sidecar with no setup, \
             and a program would take this one for it. Give the team a namespace of its own, such as \"team\"",
            path.display(),
            crate::local::LOCAL_NAMESPACE
        )));
    }
    Ok((settings, folder))
}

/// Builds the engine from settings `read` gave, talking to other machines
/// over QUIC. The replica id and namespace come from the same settings, so
/// they cannot differ from what the engine runs under.
///
/// The storage folder's `replica` file is written here, the first time, once
/// the engine has taken the data under that number.
pub async fn start(mut settings: SdkConfig, folder: &Path, claim: &Claim) -> Result<(Arc<Client>, ReplicaId, String)> {
    let (replica, namespace) = (ReplicaId(settings.replica), settings.namespace.clone());
    settings.listen = settled_listen(&settings.listen);
    let client = Client::from_config(settings, folder).await?;
    if let Some((holder, number)) = &claim.to_note {
        if let Err(e) = crate::local::write_safely(holder, "replica", &number.to_string()) {
            client.shutdown().await;
            return Err(e);
        }
    }
    Ok((Arc::new(client), replica, namespace))
}

/// Takes the machine whose settings are at `path` out of its team for good.
///
/// Its sidecar has to be stopped first: the storage is claimed as a sidecar
/// would claim it. The engine is started from the settings, with nothing
/// served, and once another machine is connected it hands over all the
/// allowance it holds on every budget (see `orvu_runtime::NodeHandle::leave`),
/// waiting at most `wait`. Then it stops.
///
/// A machine that has never run holds nothing, and is refused, as is one
/// whose settings keep nothing on disk.
pub async fn leave(path: &Path, wait: std::time::Duration) -> Result<orvu_runtime::Leaving> {
    let (mut settings, folder) = read(path)?;
    if settings.storage.path.is_none() {
        return Err(SdkError::Config(
            "these settings keep nothing on disk, so a machine started from them holds nothing to hand over".into(),
        ));
    }
    // What would turn on the log or the page is left out: nothing is served.
    crate::observe::Observe::take(&mut settings, &folder).map_err(SdkError::Config)?;
    let claim = claim(&mut settings, &folder)?;
    if claim.to_note.is_some() {
        return Err(SdkError::Config(format!(
            "machine {} has never run from these settings, so it holds nothing to hand over",
            settings.replica
        )));
    }
    settings.listen = settled_listen(&settings.listen);
    let read = |what: &str, file: &Path| {
        std::fs::read_to_string(file)
            .map_err(|e| SdkError::Config(format!("cannot read {what} {}: {e}", file.display())))
    };
    let tls = settings
        .security
        .as_ref()
        .ok_or_else(|| SdkError::Config("[security] cert/key/ca required for QUIC".into()))?;
    let cert = read("the certificate", &folder.join(&tls.cert))?;
    let key = read("the key", &folder.join(&tls.key))?;
    let ca = read("the CA certificate", &folder.join(&tls.ca))?;
    orvu_transport::install_default_crypto();
    let mut config = settings.into_node_config()?;
    let transport = Arc::new(orvu_transport::QuicTransport::with_times(&cert, &key, &ca, config.link)?);
    config.store_path = config.store_path.map(|store| folder.join(store));
    let node = orvu_runtime::Node::spawn(transport, config)
        .await
        .map_err(|e| SdkError::Config(format!("cannot start: {e}")))?;
    let left = node.leave(wait).await;
    node.shutdown().await;
    drop(claim);
    left.map_err(|e| SdkError::Config(e.to_string()))
}

/// `listen_on` with the real network, saying what it changed.
fn settled_listen(listen: &str) -> String {
    let (listen, said) = listen_on(listen, |address| std::net::UdpSocket::bind(address).map(drop));
    if let Some(said) = said {
        tracing::warn!("{said}");
        crate::say::line(format_args!("orvud: {said}"));
    }
    listen
}

/// The address to listen on for the other machines. `[::]` takes both kinds
/// of address, but on a machine with IPv6 switched off it cannot be used:
/// then every IPv4 address is listened on instead, and the second part says
/// so. `bind` tries an address, as the engine will. Any other address is
/// left as it is, and so is `[::]` when IPv4 cannot be listened on either,
/// such as when the port is taken: the engine then says why.
pub fn listen_on(listen: &str, bind: impl Fn(&str) -> std::io::Result<()>) -> (String, Option<String>) {
    let Some(port) = listen.strip_prefix("[::]:") else {
        return (listen.to_string(), None);
    };
    let Err(why) = bind(listen) else {
        return (listen.to_string(), None);
    };
    let instead = format!("0.0.0.0:{port}");
    match bind(&instead) {
        Ok(()) => {
            let said = format!(
                "cannot listen on {listen} ({why}), which takes IPv6 as well as IPv4; listening on {instead}, IPv4 only, instead"
            );
            (instead, Some(said))
        }
        Err(_) => (listen.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    // A machine with IPv6 switched off cannot listen on `[::]`.
    #[test]
    fn with_no_ipv6_every_ipv4_address_is_listened_on() {
        let tried = std::cell::RefCell::new(Vec::new());
        let no_ipv6 = |address: &str| {
            tried.borrow_mut().push(address.to_string());
            if address.starts_with('[') { Err(Error::new(ErrorKind::Unsupported, "no IPv6")) } else { Ok(()) }
        };
        let (listen, said) = listen_on("[::]:7440", no_ipv6);
        assert_eq!(listen, "0.0.0.0:7440");
        let said = said.expect("it says so");
        assert!(said.contains("[::]:7440") && said.contains("0.0.0.0:7440") && said.contains("no IPv6"), "{said}");
        assert_eq!(*tried.borrow(), ["[::]:7440", "0.0.0.0:7440"]);
    }

    #[test]
    fn an_address_that_works_is_kept() {
        let fine = |_: &str| Ok(());
        assert_eq!(listen_on("[::]:7440", fine), ("[::]:7440".to_string(), None));
        // Only `[::]` is ever changed. Any other address is the one asked for.
        let never = |_: &str| Err(Error::new(ErrorKind::Unsupported, "no"));
        for given in ["10.0.0.5:7440", "[fd00::5]:7440", "0.0.0.0:9000", "box:7440"] {
            assert_eq!(listen_on(given, never), (given.to_string(), None));
        }
    }

    // When IPv4 fails too, such as because the port is taken, the address
    // stays as it was, and the engine says why it cannot listen.
    #[test]
    fn nothing_is_changed_when_ipv4_fails_too() {
        let taken = |_: &str| Err(Error::new(ErrorKind::AddrInUse, "taken"));
        assert_eq!(listen_on("[::]:7440", taken), ("[::]:7440".to_string(), None));
    }
}
