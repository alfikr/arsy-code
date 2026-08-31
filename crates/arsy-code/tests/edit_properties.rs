use arsy_code::edit::{apply, workspace_version, EditAddress, EditOperation, EditTransaction};
use proptest::prelude::*;
use std::{collections::BTreeMap, fs, path::Path};

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn failed_edit_sequences_leave_the_workspace_byte_identical(
        contents in prop::collection::vec("[a-z]{1,32}", 1..8),
        targets in prop::collection::vec(any::<prop::sample::Index>(), 0..12),
        replacements in prop::collection::vec("[A-Z]{1,16}", 1..12),
    ) {
        let temp = tempfile::tempdir().unwrap();
        for (index, content) in contents.iter().enumerate() {
            fs::write(temp.path().join(format!("file-{index}")), content).unwrap();
        }
        let before = snapshot(temp.path());
        let mut operations = targets
            .iter()
            .enumerate()
            .map(|(index, target)| EditOperation {
                path: format!("file-{}", target.index(contents.len())).into(),
                address: EditAddress::TextAnchor {
                    needle: contents[target.index(contents.len())].clone(),
                    occurrence: None,
                },
                replacement: replacements[index % replacements.len()].as_bytes().to_vec(),
            })
            .collect::<Vec<_>>();
        operations.push(EditOperation {
            path: "missing".into(),
            address: EditAddress::TextAnchor {
                needle: "absent".into(),
                occurrence: None,
            },
            replacement: b"never written".to_vec(),
        });

        let transaction = EditTransaction {
            base: workspace_version(temp.path()).unwrap(),
            operations,
        };
        prop_assert!(apply(temp.path(), &transaction).is_err());
        prop_assert_eq!(snapshot(temp.path()), before);
    }
}
