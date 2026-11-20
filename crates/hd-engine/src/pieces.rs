//! The pieces a file is fetched in, one connection each, and the state that lets a download go on after a pause or a restart.

use serde::{Deserialize, Serialize};

/// The smallest piece worth a connection of its own.
pub const MIN_PIECE: u64 = 1 << 20;

/// A byte range `[start, end)` of the file and how much of it has been written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Piece {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

impl Piece {
    pub fn next(&self) -> u64 {
        self.start + self.done
    }
    pub fn left(&self) -> u64 {
        self.end.saturating_sub(self.next())
    }
    pub fn finished(&self) -> bool {
        self.left() == 0
    }
}

/// `total` bytes in at most `connections` pieces of at least [`MIN_PIECE`] (one piece for a small file).
pub fn plan(total: u64, connections: usize) -> Vec<Piece> {
    let n = (total / MIN_PIECE).clamp(1, connections.max(1) as u64);
    let size = total / n;
    (0..n)
        .map(|i| {
            let start = i * size;
            let end = if i + 1 == n { total } else { start + size };
            Piece { start, end, done: 0 }
        })
        .collect()
}

/// A connection that is free takes half of what is left of the biggest unfinished piece: the piece ends at the middle of its rest, and
/// the second half is the new piece. `None` when nothing left is worth splitting (`2 * MIN_PIECE`).
pub fn split_biggest(pieces: &mut Vec<Piece>) -> Option<usize> {
    let (i, p) = pieces.iter().enumerate().filter(|(_, p)| p.left() >= 2 * MIN_PIECE).max_by_key(|(_, p)| p.left())?;
    let mid = p.next() + p.left() / 2;
    let new = Piece { start: mid, end: p.end, done: 0 };
    pieces[i].end = mid;
    pieces.push(new);
    Some(pieces.len() - 1)
}

/// What is kept beside an unfinished download (`<file>.hdpart.json`): enough to go on with it later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub url: String,
    /// The name it will have once finished.
    pub name: String,
    /// The size the server said; `None` for a download of unknown size (it cannot go on after a restart, only start again).
    pub total: Option<u64>,
    pub pieces: Vec<Piece>,
    /// The server's `ETag` or `Last-Modified`: a file that changed on the server meanwhile is fetched from the start.
    #[serde(default)]
    pub validator: Option<String>,
}

impl Saved {
    pub fn done(&self) -> u64 {
        self.pieces.iter().map(|p| p.done).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_split_into_pieces_worth_a_connection() {
        assert_eq!(plan(500, 8), vec![Piece { start: 0, end: 500, done: 0 }], "small: one piece");
        let p = plan(10 * MIN_PIECE + 3, 4);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].start, 0);
        assert_eq!(p.last().unwrap().end, 10 * MIN_PIECE + 3);
        assert!(p.windows(2).all(|w| w[0].end == w[1].start), "no gap, no overlap");
        assert_eq!(plan(3 * MIN_PIECE, 8).len(), 3, "no piece under the minimum");
        assert_eq!(plan(0, 4), vec![Piece { start: 0, end: 0, done: 0 }]);
    }

    #[test]
    fn a_free_connection_takes_half_of_the_biggest_rest() {
        let mut p = vec![Piece { start: 0, end: 10 * MIN_PIECE, done: 2 * MIN_PIECE }, Piece { start: 10 * MIN_PIECE, end: 12 * MIN_PIECE, done: 0 }];
        let i = split_biggest(&mut p).unwrap();
        assert_eq!(p[0], Piece { start: 0, end: 6 * MIN_PIECE, done: 2 * MIN_PIECE });
        assert_eq!(p[i], Piece { start: 6 * MIN_PIECE, end: 10 * MIN_PIECE, done: 0 });
        let mut small = vec![Piece { start: 0, end: MIN_PIECE, done: 0 }];
        assert_eq!(split_biggest(&mut small), None);
    }
}
