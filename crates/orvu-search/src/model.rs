//! The model: fetching its files, checking them and loading them.

use model2vec_rs::model::StaticModel;
use std::path::Path;
use std::time::Duration;

/// A file of a model, and the SHA-256 of what it has to hold, written out
/// in hex.
#[derive(Clone, Copy, Debug)]
pub struct Pinned {
    pub name: &'static str,
    pub sha256: &'static str,
}

/// Where potion-base-8M is fetched from: one version of it, which does not
/// change.
pub const POTION_SOURCE: &str =
    "https://huggingface.co/minishlab/potion-base-8M/resolve/bf8b056651a2c21b8d2565580b8569da283cab23/";

/// The files of potion-base-8M.
pub const POTION: [Pinned; 3] = [
    Pinned { name: "config.json", sha256: "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553" },
    Pinned { name: "tokenizer.json", sha256: "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50" },
    Pinned { name: "model.safetensors", sha256: "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2" },
];

/// The largest file that is fetched. The model's largest is about 30 MB.
const LARGEST: u64 = 100 * 1024 * 1024;

/// How long fetching one file may take, from asking to the last byte. Three
/// files take at most nine minutes, less than the ten a search waits.
const PATIENCE: Duration = Duration::from_secs(180);

/// A model, loaded.
pub struct Model(StaticModel);

impl Model {
    /// The model whose files are in `folder`: `config.json`,
    /// `tokenizer.json` and `model.safetensors`.
    pub fn open(folder: &Path) -> Result<Model, String> {
        Self::load(folder, |_, _| Ok(()))
    }

    /// Like `open`, and each file has to be the one it is pinned to. The
    /// model is made from the very bytes that were checked.
    pub fn open_pinned(folder: &Path, files: &[Pinned]) -> Result<Model, String> {
        Self::load(folder, |name, bytes| match files.iter().find(|file| file.name == name) {
            Some(file) if sha256(bytes) == file.sha256 => Ok(()),
            Some(file) => Err(format!(
                "{} is not the one expected: its SHA-256 is {}, not {}",
                folder.join(name).display(),
                sha256(bytes),
                file.sha256
            )),
            None => Err(format!("{name} is not one of the files pinned")),
        })
    }

    fn load(folder: &Path, check: impl Fn(&str, &[u8]) -> Result<(), String>) -> Result<Model, String> {
        let read = |name: &str| {
            let path = folder.join(name);
            let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            check(name, &bytes)?;
            Ok::<_, String>(bytes)
        };
        let (tokenizer, table, config) = (read("tokenizer.json")?, read("model.safetensors")?, read("config.json")?);
        StaticModel::from_bytes(tokenizer, table, config, Some(true))
            .map(Model)
            .map_err(|e| format!("the files in {} are not a model that can be used: {e}", folder.display()))
    }

    /// The vectors of `texts`, in the same order. Each is one long, so how
    /// close two are is the sum of their numbers multiplied pairwise. A text
    /// with no word the model knows gets one of zeros, which is close to
    /// nothing.
    pub fn vectors(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if texts.is_empty() {
            return Vec::new();
        }
        self.0.encode(texts)
    }
}

/// Makes sure `folder` holds each of `files`, as it is pinned, fetching from
/// `source`, an address ending in `/`, any that are missing or not right. A
/// file is written beside its place and moved there only once it is whole and
/// checked, so a fetch that is stopped partway leaves nothing that looks
/// finished.
pub fn fetch(folder: &Path, source: &str, files: &[Pinned]) -> Result<(), String> {
    std::fs::create_dir_all(folder).map_err(|e| format!("cannot make {}: {e}", folder.display()))?;
    for file in files {
        let place = folder.join(file.name);
        if std::fs::read(&place).is_ok_and(|held| sha256(&held) == file.sha256) {
            continue;
        }
        let address = format!("{source}{}", file.name);
        let body = get(&address)?;
        let got = sha256(&body);
        if got != file.sha256 {
            return Err(format!(
                "{address} sent a file that is not the one expected: its SHA-256 is {got}, not {}",
                file.sha256
            ));
        }
        let beside = folder.join(format!(".{}.{}", file.name, std::process::id()));
        let written = std::fs::write(&beside, &body).and_then(|()| std::fs::rename(&beside, &place));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&beside);
            return Err(format!("cannot keep {} in {}: {e}", file.name, folder.display()));
        }
    }
    Ok(())
}

/// What is at `address`, at most `LARGEST` bytes of it.
fn get(address: &str) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(PATIENCE)).build().into();
    let mut answer = agent.get(address).call().map_err(|e| format!("cannot fetch {address}: {e}"))?;
    answer
        .body_mut()
        .with_config()
        .limit(LARGEST)
        .read_to_vec()
        .map_err(|e| format!("cannot fetch {address}: {e}"))
}

/// The SHA-256 of `bytes`, in hex.
fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
