//! Finding text by meaning, with the made-up model, and fetching a model.

use orvu_search::{fetch, Index, Model, Pinned};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn tiny() -> (tempfile::TempDir, Model) {
    let folder = tempfile::tempdir().unwrap();
    orvu_search::tiny::write(folder.path()).unwrap();
    let model = Model::open(folder.path()).unwrap();
    (folder, model)
}

fn parts(texts: &[&str]) -> Vec<(String, String)> {
    texts.iter().enumerate().map(|(n, text)| ((n + 1).to_string(), text.to_string())).collect()
}

// Texts that mean the same are close; texts that share no meaning are not.
#[test]
fn texts_that_mean_the_same_are_close() {
    let (_folder, model) = tiny();
    let v = model.vectors(&["refund policy".into(), "money back".into(), "deploy on friday".into(), "".into()]);
    let close = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    assert!(close(&v[0], &v[1]) > 0.99);
    assert!(close(&v[0], &v[2]).abs() < 0.01);
    assert!(v[3].iter().all(|n| *n == 0.0), "a text with no known word is close to nothing");
}

// The closest come first, at most as many as asked for, and a text that has
// nothing in common with the question is not found at all.
#[test]
fn the_closest_texts_are_found_first() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    index.put("notes", parts(&["money back before the deploy", "deploy on friday", "the cat sat"]), &model);
    index.put("doc", parts(&["a refund is paid within days, money back, repay", "sales rose"]), &model);
    let found = index.search("what is the refund policy", &model, 5, |_| true);
    let got: Vec<(&str, &str)> = found.iter().map(|f| (f.object.as_str(), f.part.as_str())).collect();
    assert_eq!(got, [("doc", "1"), ("notes", "1")]);
    assert!(found[0].score >= found[1].score && found[0].score <= 1.0);
    assert_eq!(index.search("refund", &model, 1, |_| true).len(), 1);
    assert!(index.search("the dog", &model, 5, |_| true).is_empty());
}

// A part is found once, with its closest text, though several of its texts
// are close: a long paragraph searched in pieces does not crowd out the rest.
#[test]
fn a_part_is_found_once_with_its_closest_text() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    let pieces = vec![
        ("1".to_string(), "refund deploy".to_string()),
        ("1".to_string(), "refund money".to_string()),
        ("1".to_string(), "money back".to_string()),
    ];
    index.put("doc", pieces, &model);
    index.put("note", parts(&["repay"]), &model);
    let found = index.search("refund", &model, 2, |_| true);
    let got: Vec<(&str, &str, &str)> = found.iter().map(|f| (f.object.as_str(), f.part.as_str(), f.text.as_str())).collect();
    assert_eq!(got, [("doc", "1", "refund money"), ("note", "1", "repay")]);
}

// A question whose vector is not made of numbers finds nothing.
#[test]
fn a_broken_question_finds_nothing() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    index.put("doc", parts(&["refund"]), &model);
    assert!(index.search_vector(&[f32::NAN; 6], 5, |_| true).is_empty());
}

// The closest few of many are the same, and in the same order, as when all
// are put in order, ties included.
#[test]
fn the_closest_few_of_many_are_the_closest() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    let mixes = ["refund", "refund deploy", "refund deploy deploy", "refund sales", "deploy", "refund money"];
    for n in 0..40 {
        index.put(&format!("doc-{n:02}"), parts(&[mixes[n % mixes.len()]]), &model);
    }
    let all = index.search("refund", &model, 50, |_| true);
    for limit in [1, 3, 5, 7, 13] {
        let few = index.search("refund", &model, limit, |_| true);
        assert_eq!(few, all[..limit.min(all.len())], "{limit}");
    }
}

// Only the objects `keep` lets through are looked at.
#[test]
fn only_the_objects_kept_are_found() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    index.put("open", parts(&["money back"]), &model);
    index.put("private", parts(&["refund"]), &model);
    let found = index.search("refund", &model, 5, |object| object != "private");
    assert_eq!(found.iter().map(|f| f.object.as_str()).collect::<Vec<_>>(), ["open"]);
}

// Whether an object may be read is asked once for the object, not once for
// each of its texts.
#[test]
fn each_object_is_asked_about_once() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    index.put("doc", parts(&["refund", "money back", "repay", "deploy"]), &model);
    index.put("other", parts(&["refund"]), &model);
    let asked = std::cell::RefCell::new(Vec::new());
    let found = index.search("refund", &model, 5, |object| {
        asked.borrow_mut().push(object.to_string());
        true
    });
    assert_eq!(found.len(), 4);
    let mut asked = asked.into_inner();
    asked.sort();
    assert_eq!(asked, ["doc", "other"]);
}

// Putting an object again replaces its texts, and removing it takes them out.
#[test]
fn an_object_put_again_has_only_its_new_texts() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    index.put("doc", parts(&["refund", "deploy"]), &model);
    index.put("doc", parts(&["sales rose"]), &model);
    assert_eq!(index.len(), 1);
    assert!(index.search("refund", &model, 5, |_| true).is_empty());
    assert_eq!(index.search("revenue", &model, 5, |_| true)[0].text, "sales rose");
    index.put("other", parts(&["refund"]), &model);
    index.remove("doc");
    assert_eq!(index.objects().into_iter().collect::<Vec<_>>(), ["other"]);
}

// What an object has can be worked on apart from the index: whether it
// would change, and its vectors, with those it had kept.
#[test]
fn an_object_can_be_prepared_apart_from_the_index() {
    let (_folder, model) = tiny();
    let mut index = Index::default();
    index.put("doc", parts(&["refund", "deploy"]), &model);
    assert!(index.holds("doc", &parts(&["refund", "deploy"])));
    assert!(!index.holds("doc", &parts(&["refund"])));
    assert!(!index.holds("doc", &parts(&["deploy", "refund"])));
    assert!(index.holds("other", &[]));
    let new = parts(&["sales rose", "refund", "sales rose"]);
    let prepared = index.missing("doc", &new).prepare(&model);
    index.put_prepared("doc", new, prepared);
    assert_eq!(index.search("revenue", &model, 5, |_| true)[0].part, "1");
    assert_eq!(index.search("money", &model, 5, |_| true)[0].part, "2");
    assert_eq!(index.len(), 3);
    assert_eq!(index.search("revenue", &model, 5, |_| true).len(), 2, "both pieces with that text have its vector");
}

// The made-up model the Python and TypeScript tests load is the one this
// crate writes, so the two cannot drift apart.
#[test]
fn the_made_up_model_kept_for_other_tests_is_the_one_written_here() {
    let kept = Path::new(env!("CARGO_MANIFEST_DIR")).join("tiny-model");
    let (folder, _model) = tiny();
    for name in ["config.json", "tokenizer.json", "model.safetensors"] {
        let (written, there) = (std::fs::read(folder.path().join(name)).unwrap(), std::fs::read(kept.join(name)));
        assert_eq!(Some(written), there.ok(), "{name} in {} is not the one written", kept.display());
    }
}

/// A web server on this machine that serves `files` and counts the requests
/// it answers, one at a time, until the test ends.
fn serve(files: Vec<(&'static str, Vec<u8>)>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}/", listener.local_addr().unwrap());
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut first = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            reader.read_line(&mut first).unwrap();
            // The rest of the request is not needed.
            while reader.read_line(&mut String::new()).unwrap() > 2 {}
            counted.fetch_add(1, Ordering::SeqCst);
            let path = first.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
            match files.iter().find(|(name, _)| *name == path) {
                Some((_, body)) => {
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    stream.write_all(body).unwrap();
                }
                None => write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap(),
            }
        }
    });
    (address, asked)
}

fn pinned(name: &'static str, body: &[u8]) -> Pinned {
    let sha: String = ring_sha(body);
    Pinned { name, sha256: Box::leak(sha.into_boxed_str()) }
}

fn ring_sha(body: &[u8]) -> String {
    // Worked out apart from the code under test, with the system's own tool.
    let mut child = std::process::Command::new("sha256sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap().split_whitespace().next().unwrap().to_string()
}

fn names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(folder).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

// The files are fetched once, checked, and kept where they were asked for.
#[test]
fn a_model_is_fetched_once_and_checked() {
    let (one, two) = (b"first file".to_vec(), b"second file".to_vec());
    let files = [pinned("a.json", &one), pinned("b.bin", &two)];
    let (address, asked) = serve(vec![("a.json", one.clone()), ("b.bin", two.clone())]);
    let home = tempfile::tempdir().unwrap();
    let folder: PathBuf = home.path().join("models").join("tiny");
    fetch(&folder, &address, &files).unwrap();
    assert_eq!(std::fs::read(folder.join("a.json")).unwrap(), one);
    assert_eq!(std::fs::read(folder.join("b.bin")).unwrap(), two);
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    fetch(&folder, &address, &files).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 2, "files already right are not fetched again");
    std::fs::write(folder.join("b.bin"), b"spoiled").unwrap();
    fetch(&folder, &address, &files).unwrap();
    assert_eq!(std::fs::read(folder.join("b.bin")).unwrap(), two, "a file that is not right is fetched again");
    assert_eq!(names(&folder), ["a.json", "b.bin"]);
}

// A file that is not the one pinned is refused, and nothing of it is kept.
#[test]
fn a_file_that_is_not_the_one_pinned_is_refused() {
    let files = [pinned("a.json", b"the right file")];
    let (address, _) = serve(vec![("a.json", b"another file".to_vec())]);
    let home = tempfile::tempdir().unwrap();
    let why = fetch(home.path(), &address, &files).unwrap_err();
    assert!(why.contains("not the one expected"), "{why}");
    assert!(names(home.path()).is_empty(), "{:?}", names(home.path()));
}

// A model loaded by its pins is loaded from exactly the bytes that were
// checked, and one whose file is not the one pinned is refused.
#[test]
fn a_model_is_loaded_only_as_it_is_pinned() {
    let folder = tempfile::tempdir().unwrap();
    orvu_search::tiny::write(folder.path()).unwrap();
    let pins: Vec<Pinned> = ["config.json", "tokenizer.json", "model.safetensors"]
        .into_iter()
        .map(|name| pinned(name, &std::fs::read(folder.path().join(name)).unwrap()))
        .collect();
    assert!(Model::open_pinned(folder.path(), &pins).is_ok());
    std::fs::write(folder.path().join("config.json"), br#"{"model_type":"model2vec","hidden_dim":6,"normalize":false}"#).unwrap();
    let why = Model::open_pinned(folder.path(), &pins).err().unwrap();
    assert!(why.contains("config.json") && why.contains("not the one expected"), "{why}");
}

// A file the server does not have, or a server that is not there, is said.
#[test]
fn a_file_that_cannot_be_fetched_is_said() {
    let (address, _) = serve(vec![]);
    let home = tempfile::tempdir().unwrap();
    let why = fetch(home.path(), &address, &[pinned("a.json", b"x")]).unwrap_err();
    assert!(why.contains("cannot fetch") && why.contains("404"), "{why}");
    let gone = { TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap() };
    let why = fetch(home.path(), &format!("http://{gone}/"), &[pinned("a.json", b"x")]).unwrap_err();
    assert!(why.contains("cannot fetch"), "{why}");
}

// The real model, fetched from where the sidecar fetches it. It needs the
// internet, so it runs only when asked for: cargo test -p orvu-search -- --ignored
#[test]
#[ignore]
fn the_real_model_finds_a_refund_policy() {
    let home = tempfile::tempdir().unwrap();
    fetch(home.path(), orvu_search::POTION_SOURCE, &orvu_search::POTION).unwrap();
    let model = Model::open_pinned(home.path(), &orvu_search::POTION).unwrap();
    let mut index = Index::default();
    let texts = ["The customer can get their money back within 30 days.", "Deploy is planned for Friday.", "Sales rose 4%."];
    index.put("notes", parts(&texts), &model);
    assert_eq!(index.search("what is the refund policy?", &model, 1, |_| true)[0].part, "1");
}
