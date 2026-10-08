//! Selected results. By full path, so a refresh that moves rows keeps the right entries
//! selected; rows are only used to know what a click means.

#[derive(Debug, Default)]
pub struct Selection {
    /// In the order they were selected
    pub paths: Vec<String>,
    /// Row a Shift+click selects from
    anchor: Option<usize>,
}

impl Selection {
    pub fn contains(&self, path: &str) -> bool {
        self.paths.iter().any(|p| p == path)
    }

    pub fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }

    /// Just `row`.
    pub fn set(&mut self, row: usize, path: Option<String>) {
        self.paths = path.into_iter().collect();
        self.anchor = Some(row);
    }

    /// Changes the selection for a click on `row`: `ctrl` adds or removes the row, `shift`
    /// selects from the row clicked before, both add that range. `path` gives the path of a row
    /// if it is known.
    pub fn click(
        &mut self,
        row: usize,
        ctrl: bool,
        shift: bool,
        path: impl Fn(usize) -> Option<String>,
    ) {
        let Some(clicked) = path(row) else {
            return;
        };
        match self.anchor.filter(|_| shift) {
            Some(anchor) => {
                if !ctrl {
                    self.paths.clear();
                }
                // From the anchor towards the clicked row
                let range: Box<dyn Iterator<Item = usize>> = if anchor <= row {
                    Box::new(anchor..=row)
                } else {
                    Box::new((row..=anchor).rev())
                };
                for p in range.filter_map(&path) {
                    if !self.contains(&p) {
                        self.paths.push(p);
                    }
                }
            }
            None if ctrl => {
                match self.paths.iter().position(|p| *p == clicked) {
                    Some(at) => {
                        self.paths.remove(at);
                    }
                    None => self.paths.push(clicked),
                }
                self.anchor = Some(row);
            }
            None => self.set(row, Some(clicked)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(row: usize) -> Option<String> {
        (row < 100).then(|| format!("C:\\{}", row))
    }

    fn rows(selection: &Selection) -> Vec<usize> {
        selection
            .paths
            .iter()
            .map(|p| p[3..].parse().unwrap())
            .collect()
    }

    #[test]
    fn clicks() {
        let mut s = Selection::default();
        s.click(1, false, false, path);
        assert_eq!(rows(&s), [1]);
        // Shift selects the range from the anchor, Ctrl takes one out of it
        s.click(4, false, true, path);
        assert_eq!(rows(&s), [1, 2, 3, 4]);
        s.click(2, true, false, path);
        assert_eq!(rows(&s), [1, 3, 4]);
        // The anchor moved to row 2, Ctrl+Shift adds that range to what is there
        s.click(6, true, true, path);
        assert_eq!(rows(&s), [1, 3, 4, 2, 5, 6]);
        // Shift alone replaces it, upwards too
        s.click(0, false, true, path);
        assert_eq!(rows(&s), [2, 1, 0]);
        // A plain click selects one row, also one that was selected before
        s.click(1, false, false, path);
        assert_eq!(rows(&s), [1]);
        // Rows without a path do nothing
        s.click(200, false, false, path);
        assert_eq!(rows(&s), [1]);
    }
}
