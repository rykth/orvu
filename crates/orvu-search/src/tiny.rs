//! A made-up model, for tests. It knows a few dozen words in groups: words
//! of one group mean the same, and words of different groups have nothing in
//! common. It is loaded like a real one, from the same three files.

use std::path::Path;

/// The words the model knows, in their groups.
pub const GROUPS: &[&[&str]] = &[
    &["refund", "refunds", "money", "back", "repay", "returns"],
    &["deploy", "release", "ship", "friday", "launch", "when"],
    &["sales", "revenue", "rose", "grew", "income", "quarter"],
    &["password", "credentials", "rotated", "key", "secret", "changed"],
    &["holiday", "away", "leave", "vacation", "office", "closed"],
    &["report", "summary", "draft", "chapter", "written", "document"],
];

/// Writes the model's files into `folder`, which must be there.
pub fn write(folder: &Path) -> std::io::Result<()> {
    let words: Vec<(&str, usize)> =
        GROUPS.iter().enumerate().flat_map(|(group, words)| words.iter().map(move |word| (*word, group))).collect();
    let width = GROUPS.len();
    // The first piece is the one for words the model does not know.
    let vocabulary: Vec<String> = std::iter::once("\"[UNK]\":0".to_string())
        .chain(words.iter().enumerate().map(|(n, (word, _))| format!("\"{word}\":{}", n + 1)))
        .collect();
    let tokenizer = format!(
        concat!(
            r#"{{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"#,
            r#""normalizer":{{"type":"Lowercase"}},"pre_tokenizer":{{"type":"Whitespace"}},"#,
            r#""post_processor":null,"decoder":null,"#,
            r#""model":{{"type":"WordLevel","vocab":{{{}}},"unk_token":"[UNK]"}}}}"#
        ),
        vocabulary.join(",")
    );
    let mut table = vec![0f32; width];
    for (_, group) in &words {
        let mut row = vec![0f32; width];
        row[*group] = 1.0;
        table.extend(row);
    }
    let bytes: Vec<u8> = table.iter().flat_map(|n| n.to_le_bytes()).collect();
    let mut header = format!(
        r#"{{"embeddings":{{"dtype":"F32","shape":[{},{width}],"data_offsets":[0,{}]}}}}"#,
        words.len() + 1,
        bytes.len()
    );
    while header.len() % 8 != 0 {
        header.push(' ');
    }
    let mut safetensors = (header.len() as u64).to_le_bytes().to_vec();
    safetensors.extend(header.as_bytes());
    safetensors.extend(bytes);
    let config = format!(r#"{{"model_type":"model2vec","hidden_dim":{width},"normalize":true}}"#);
    std::fs::write(folder.join("tokenizer.json"), tokenizer)?;
    std::fs::write(folder.join("model.safetensors"), safetensors)?;
    std::fs::write(folder.join("config.json"), config)
}
