//! Writes the made-up model into a folder, for the tests of the Python and
//! TypeScript libraries:
//!
//!     cargo run -p orvu-search --example tiny_model -- crates/orvu-search/tiny-model

fn main() {
    let folder = std::env::args_os().nth(1).expect("give the folder to write the model into");
    std::fs::create_dir_all(&folder).expect("the folder could not be made");
    orvu_search::tiny::write(std::path::Path::new(&folder)).expect("the model could not be written");
}
