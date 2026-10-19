# orvu-search

Finding text by what it means. The sidecar uses it for search by meaning
over a team's notes and documents.

A model turns a piece of text into a list of numbers, its vector, so that
texts that mean much the same get vectors that point the same way. An index
keeps the vectors of many texts and finds the ones closest to a question's.

## What is inside

| Piece | What it is |
|---|---|
| `Model` | A model, loaded from its three files with `open`, or with `open_pinned`, which loads it only from bytes that match their SHA-256. `vectors` turns texts into vectors |
| `fetch` | Fetches a model's files into a folder, and checks each against the SHA-256 it should have |
| `POTION`, `POTION_SOURCE` | The files of potion-base-8M, and where one version of it is fetched from |
| `Index` | Texts in parts of objects, with their vectors. `put`, `remove`, `search`; `holds`, `missing` and `put_prepared` to work vectors out apart from it |
| `Missing`, `Prepared` | The texts an object has no vector for, and their vectors once worked out |
| `Found` | A text a search found, with how close it is, from 0 to 1 |
| `tiny` | A made-up model for tests, which knows a few dozen words in groups |
| `tiny-model/` | That model's files, for the tests of the Python and TypeScript libraries |

## How to use it

```rust
use orvu_search::{Index, Model};

// The made-up model: "refund", "money" and "back" mean the same to it.
let folder = tempfile::tempdir().unwrap();
orvu_search::tiny::write(folder.path()).unwrap();
let model = Model::open(folder.path()).unwrap();

let mut index = Index::default();
let parts = vec![("1".to_string(), "customers get their money back".to_string()), ("2".to_string(), "we ship on friday".to_string())];
index.put("orvu/crew/doc/handbook", parts, &model);

let found = index.search("what is the refund policy", &model, 5, |_object| true);
assert_eq!(found.len(), 1);
assert_eq!(found[0].part, "1");
```

The real model is fetched once with `fetch(folder, POTION_SOURCE, &POTION)`
and loaded with `Model::open_pinned(folder, &POTION)`, which checks each
file again as it reads it. `Model::open` checks nothing, and is for a model
whose files were checked another way, such as the made-up one.

## Things to know

- **The model is potion-base-8M,** made by Minish Lab and free to use under
  the MIT licence: a table of 256 numbers for each of about 30,000 pieces of
  English words. A text's vector is the average of its pieces'. That is fast,
  well under a thousandth of a second for a short text, and needs no other
  program. It was tried against small language models of the usual kind,
  on thirty questions about thirty notes. The best of them, bge-small, put
  the right note first two times more in thirty, and among the first three
  one time fewer, at a thousand times the time and four times the size.
- **It goes by the words of a text, not their order,** so "the dog bit the
  man" and "the man bit the dog" mean the same to it. It knows English.
- **A text with no word the model knows is close to nothing,** and is never
  found. With the real model nearly every other text has a little in common
  with any question, so the last of what is found may have little to do with
  it. Its score shows how close each is.
- **The version fetched is fixed,** and a file that is not exactly the one
  pinned is refused. A file is kept only once it is whole and checked.
- **`cargo test -p orvu-search -- --ignored`** also fetches the real model
  from the internet and checks that it finds a refund policy. The other tests
  need no internet.
- **The made-up model in `tiny-model/` is written by** `cargo run -p
  orvu-search --example tiny_model -- crates/orvu-search/tiny-model`. A test
  checks that it is the one this crate writes.
