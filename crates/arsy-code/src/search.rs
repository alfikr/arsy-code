use crate::resource::{ResolveError, Workspace};
use arsy_kernel::domain::ResourceRef;
use ignore::WalkBuilder;
use std::{fmt, io};

#[derive(Debug, Eq, PartialEq)]
pub struct SearchHit {
    pub resource: ResourceRef,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
}

impl Workspace {
    pub fn search(
        &self,
        needle: &str,
        max_results: usize,
        max_files: usize,
        max_file_bytes: u64,
    ) -> Result<SearchResults, SearchError> {
        if needle.is_empty() {
            return Err(SearchError::EmptyNeedle);
        }

        let mut results = SearchResults {
            hits: Vec::new(),
            truncated: false,
        };
        let mut files = 0;
        for entry in WalkBuilder::new(self.path()).standard_filters(true).build() {
            let entry = entry.map_err(SearchError::Walk)?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if files == max_files {
                results.truncated = true;
                return Ok(results);
            }
            files += 1;
            let relative = entry
                .path()
                .strip_prefix(self.path())
                .expect("walk stays below root");
            let resolved = self.resolve_file(relative)?;
            let resource = resolved.resource().clone();
            let content = match resolved.read(max_file_bytes) {
                Ok(content) => content.bytes,
                Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
                    results.truncated = true;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if content.contains(&0) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&content) else {
                continue;
            };
            for (line, text) in text
                .lines()
                .enumerate()
                .filter(|(_, line)| line.contains(needle))
            {
                if results.hits.len() == max_results {
                    results.truncated = true;
                    return Ok(results);
                }
                results.hits.push(SearchHit {
                    resource: resource.clone(),
                    line: line + 1,
                    text: text.to_owned(),
                });
            }
        }
        Ok(results)
    }
}

#[derive(Debug)]
pub enum SearchError {
    EmptyNeedle,
    Walk(ignore::Error),
    Resolve(ResolveError),
    Io(io::Error),
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyNeedle => formatter.write_str("search term must not be empty"),
            Self::Walk(error) => error.fmt(formatter),
            Self::Resolve(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SearchError {}

impl From<ResolveError> for SearchError {
    fn from(value: ResolveError) -> Self {
        Self::Resolve(value)
    }
}

impl From<io::Error> for SearchError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn reads_exact_bytes_with_an_edit_precondition_digest() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"hello\0world";
        std::fs::write(temp.path().join("data"), bytes).unwrap();

        let content = Workspace::open(temp.path())
            .unwrap()
            .resolve_file("data")
            .unwrap()
            .read(32)
            .unwrap();

        assert_eq!(content.bytes, bytes);
        assert_eq!(content.digest.digest(), &Sha256::digest(bytes).as_slice());
    }

    #[test]
    fn search_honours_gitignore_skips_binary_and_reports_truncation() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(temp.path().join("visible.txt"), "needle one\nneedle two\n").unwrap();
        std::fs::write(temp.path().join("ignored.txt"), "needle ignored\n").unwrap();
        std::fs::write(temp.path().join("binary"), b"needle\0hidden").unwrap();

        let result = Workspace::open(temp.path())
            .unwrap()
            .search("needle", 1, 16, 1024)
            .unwrap();

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].resource.value(), "visible.txt");
        assert!(result.truncated);
    }
}
