use std::collections::HashSet;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Result, bail};

/// The first port in `range` that nobody has claimed and nothing listens on.
/// `claimed` covers what a probe cannot see: stopped containers and worktrees
/// whose servers are not running.
pub fn find_free(range: (u16, u16), claimed: &HashSet<u16>) -> Result<u16> {
    find_free_with(range, claimed, listening)
}

fn find_free_with(
    (first, last): (u16, u16),
    claimed: &HashSet<u16>,
    in_use: impl Fn(u16) -> bool,
) -> Result<u16> {
    match (first..=last).find(|p| !claimed.contains(p) && !in_use(*p)) {
        Some(port) => Ok(port),
        None => bail!("no free port between {first} and {last}; free one with wt down"),
    }
}

fn listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_claimed_and_listening() {
        let claimed = HashSet::from([5433, 5434]);
        let port = find_free_with((5433, 5499), &claimed, |p| p == 5435).unwrap();
        assert_eq!(port, 5436);
    }

    #[test]
    fn exact_match_only() {
        // 543 claimed must not block 5433, and vice versa.
        let claimed = HashSet::from([543, 54330]);
        assert_eq!(
            find_free_with((5433, 5499), &claimed, |_| false).unwrap(),
            5433
        );
    }

    #[test]
    fn errors_when_exhausted() {
        let claimed = HashSet::from([3001, 3002]);
        assert!(find_free_with((3001, 3002), &claimed, |_| false).is_err());
    }
}
