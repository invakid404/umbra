//! Bounded directory pages: cookie continuity, noise filtering, no whole-directory
//! materialisation.
//!
//! # Bounded means bounded at every level
//!
//! One [`page`] call issues at most [`MAX_SERVER_PAGES`] READDIR operations,
//! each capped by [`ReadDirRequest::max_count`], and returns at most `limit`
//! entries. Nothing accumulates a whole directory: the loop exists only because
//! filtered entries consume server entries without producing contract ones, and
//! it stops at the first of "page full", "server reported EOF" or "page budget
//! spent". A directory larger than the budget yields a cursor, never a stall and
//! never an unbounded allocation.
//!
//! # Cookie verifier continuity
//!
//! A NFSv4.0 cookie is only meaningful under the `cookieverf` that produced it.
//! A [`PageCursor`] therefore carries three things — the directory's identity,
//! the cookie, and the verifier — and all three are checked before a continuation
//! is issued. A changed verifier is reported as [`ErrorKind::StaleHandle`]:
//! explicit invalidation, which the contract permits, rather than an indefinitely
//! cached snapshot, which it forbids. A cursor also cannot escape its directory,
//! because the identity it was minted against must match the directory being
//! listed.
//!
//! # What the noise filter removes, and what it deliberately does not
//!
//! [`NoiseFilter`] drops `.` and `..`, which a real server returns and the
//! contract forbids surfacing, and it drops provider-private names at the run
//! anchor. It does **not** hide dotfiles generally. A `.gitignore` under the run
//! root is the tracee's data, and a listing that silently omitted it would be a
//! false answer about the directory's contents.

use umbra_core::{DirectoryEntry, DirectoryPage, ErrorKind, ListCursor, Result, UmbraError};

use crate::handle::ObjectIdentity;
use crate::identity::{blob_stat, identity_of, PinnedObject};
use crate::layout;
use crate::transport::{
    AttrMask, Deadline, DirCookie, DirEntry, DirVerifier, Fsid, RawTransport, ReadDirRequest,
};

/// Server READDIR operations one [`page`] call may issue.
///
/// Four is enough to skip the `.`, `..` and provider-private entries a page can
/// begin with while still filling an ordinary page in one or two round trips. It
/// is a hard bound, not a retry policy: exhausting it returns the entries found
/// so far plus a cursor.
pub const MAX_SERVER_PAGES: u32 = 4;

/// Bytes budgeted per entry when sizing a READDIR reply.
///
/// A name plus the `STAT` attribute set fits comfortably; the server is free to
/// return fewer entries than the budget suggests, which the paging loop handles
/// as the ordinary case rather than as an error.
pub const BYTES_PER_ENTRY: u32 = 512;

/// Names removed from an enumeration before it reaches a consumer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoiseFilter {
    private: Vec<Vec<u8>>,
}

impl NoiseFilter {
    /// Remove only the protocol noise every directory carries: `.` and `..`.
    ///
    /// This is the filter for the root and control anchors, whose contents are
    /// entirely the tracee's or the overlay's.
    pub fn protocol_only() -> Self {
        Self {
            private: Vec::new(),
        }
    }

    /// Also remove provider-private names.
    ///
    /// Used for the run anchor, which holds `.provider` beside the two contract
    /// anchors. `.provider` is this provider's own state, not run content, and
    /// the syscall matrix explicitly declines to promise anything about a
    /// consumer that mutates it.
    pub fn with_private_state() -> Self {
        Self {
            private: vec![layout::PRIVATE_DIR.to_vec()],
        }
    }

    /// Whether an entry name is filtered.
    pub fn is_noise(&self, name: &[u8]) -> bool {
        name == b"." || name == b".." || self.private.iter().any(|hidden| hidden == name)
    }
}

/// An opaque continuation for one directory in one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCursor {
    directory: ObjectIdentity,
    cookie: DirCookie,
    verifier: DirVerifier,
}

/// Cursor encoding version. A cursor from another layout fails to decode.
const CURSOR_VERSION: u8 = 1;
/// Magic prefix, so a cursor from another provider fails fast.
const CURSOR_MAGIC: &[u8; 4] = b"UDIR";
/// Encoded cursor length: magic, version, fsid pair, fileid, cookie, verifier.
const CURSOR_LEN: usize = 4 + 1 + 8 + 8 + 8 + 8 + 8;

impl PageCursor {
    /// Mint a cursor for the next page of `directory`.
    pub fn new(directory: ObjectIdentity, cookie: DirCookie, verifier: DirVerifier) -> Self {
        Self {
            directory,
            cookie,
            verifier,
        }
    }

    /// The directory this cursor belongs to.
    pub fn directory(&self) -> ObjectIdentity {
        self.directory
    }

    /// The resume cookie.
    pub fn cookie(&self) -> DirCookie {
        self.cookie
    }

    /// The verifier the cookie is only valid under.
    pub fn verifier(&self) -> DirVerifier {
        self.verifier
    }

    /// Encode into the contract's opaque continuation.
    pub fn encode(&self) -> ListCursor {
        let mut bytes = Vec::with_capacity(CURSOR_LEN);
        bytes.extend_from_slice(CURSOR_MAGIC);
        bytes.push(CURSOR_VERSION);
        bytes.extend_from_slice(&self.directory.fsid.major.to_be_bytes());
        bytes.extend_from_slice(&self.directory.fsid.minor.to_be_bytes());
        bytes.extend_from_slice(&self.directory.fileid.to_be_bytes());
        bytes.extend_from_slice(&self.cookie.0.to_be_bytes());
        bytes.extend_from_slice(&self.verifier.0);
        ListCursor(bytes)
    }

    /// Decode a continuation and prove it belongs to `directory`.
    ///
    /// A cursor minted for one directory can never be used to enumerate another,
    /// which is the contract's "never escape its directory" rule enforced rather
    /// than trusted.
    pub fn decode(cursor: &ListCursor, directory: ObjectIdentity) -> Result<Self> {
        let bytes = &cursor.0;
        if bytes.len() != CURSOR_LEN || &bytes[..4] != CURSOR_MAGIC || bytes[4] != CURSOR_VERSION {
            return Err(invalidated("the cursor was not issued by this provider"));
        }
        let word = |offset: usize| {
            let mut buffer = [0u8; 8];
            buffer.copy_from_slice(&bytes[offset..offset + 8]);
            u64::from_be_bytes(buffer)
        };
        let mut verifier = [0u8; 8];
        verifier.copy_from_slice(&bytes[37..45]);
        let decoded = Self {
            directory: ObjectIdentity {
                fsid: Fsid {
                    major: word(5),
                    minor: word(13),
                },
                fileid: word(21),
            },
            cookie: DirCookie(word(29)),
            verifier: DirVerifier(verifier),
        };
        if decoded.directory != directory {
            return Err(invalidated(
                "the cursor belongs to a different directory and cannot be continued here",
            ));
        }
        Ok(decoded)
    }
}

/// Read one bounded page of a directory.
///
/// Returns at most `limit` entries plus a continuation when more remain. The
/// server pages consumed are bounded by [`MAX_SERVER_PAGES`]; a page that fills
/// its budget on filtered entries alone returns no entries and a cursor, which is
/// a correct answer rather than an error.
pub fn page(
    transport: &mut dyn RawTransport,
    directory: &PinnedObject,
    cursor: Option<&ListCursor>,
    limit: u32,
    filter: &NoiseFilter,
    deadline: Deadline,
) -> Result<DirectoryPage> {
    if !directory.is_directory() {
        return Err(UmbraError::new(
            ErrorKind::InvalidPath,
            "list",
            "the path does not name a directory",
        ));
    }
    if limit == 0 {
        return Err(UmbraError::new(
            ErrorKind::InvalidInput,
            "list",
            "a page limit of zero requests nothing",
        ));
    }
    let resume = cursor
        .map(|cursor| PageCursor::decode(cursor, directory.identity()))
        .transpose()?;
    let mut position = resume.map_or(DirCookie(0), |cursor| cursor.cookie);
    // A first page has no verifier to honour; a continuation does, and the server
    // is asked to enforce it too by echoing it back in the request.
    let mut expected = resume.map(|cursor| cursor.verifier);
    let max_count = limit.saturating_mul(BYTES_PER_ENTRY).max(BYTES_PER_ENTRY);
    let bound = transport.limits().max_reply_bytes;
    let max_count = max_count.min(u32::try_from(bound).unwrap_or(u32::MAX));

    let mut entries = Vec::with_capacity(limit as usize);
    let mut exhausted = false;
    for _ in 0..MAX_SERVER_PAGES {
        let request = ReadDirRequest {
            cookie: position,
            verifier: expected.unwrap_or(DirVerifier([0; 8])),
            dir_count: max_count,
            max_count,
            attrs: AttrMask::STAT.union(AttrMask::RDATTR_ERROR),
        };
        let server_page = transport
            .readdir(directory.handle(), request, deadline)
            .map_err(|error| match error.status() {
                // The server invalidated the cookie itself. Reporting it as
                // anything but an invalidated cursor would invite a caller to
                // retry the same cookie forever.
                //
                // R1-011: the original facade error is carried through rather
                // than replaced. The old code built a fresh `StaleHandle` whose
                // whole context was "the server invalidated this cursor", so the
                // numeric status, the failing operation and its COMPOUND index
                // were all lost — a caller diagnosing why enumeration keeps
                // restarting had nothing left to read.
                Some(status) if status == crate::error::Nfs4Status::BAD_COOKIE => {
                    invalidated(&format!(
                        "the server invalidated this cursor ({})",
                        error.to_umbra("list").context
                    ))
                }
                _ => error.to_umbra("list"),
            })?;
        match expected {
            Some(verifier) if verifier != server_page.verifier => {
                return Err(invalidated(
                    "the directory cookie verifier changed; outstanding cursors are void",
                ))
            }
            _ => expected = Some(server_page.verifier),
        }
        if server_page.entries.is_empty() && !server_page.eof {
            // No progress and not finished: stop rather than spin. The caller
            // receives a cursor and can ask again.
            break;
        }
        // `position` advances only for entries actually taken, so a page that
        // fills mid-way leaves a cursor pointing at the first entry the caller
        // has *not* seen. Advancing past an undelivered entry would drop it.
        let mut drained = true;
        for entry in &server_page.entries {
            if entries.len() == limit as usize {
                drained = false;
                break;
            }
            position = entry.cookie;
            if filter.is_noise(entry.name.as_bytes()) {
                continue;
            }
            entries.push(translate(entry)?);
        }
        if !drained {
            // The page is full and this server page still has entries. Report a
            // cursor; `eof` on a page whose tail was not delivered says nothing
            // about what the caller has seen.
            break;
        }
        if server_page.eof {
            exhausted = true;
            break;
        }
        if entries.len() == limit as usize {
            break;
        }
    }

    let next = (!exhausted).then(|| {
        PageCursor::new(
            directory.identity(),
            position,
            expected.unwrap_or(DirVerifier([0; 8])),
        )
        .encode()
    });
    Ok(DirectoryPage { entries, next })
}

/// Translate one server entry, preserving its byte name and stable identity.
fn translate(entry: &DirEntry) -> Result<DirectoryEntry> {
    // `FATTR4_RDATTR_ERROR` is the server saying it could not answer for this
    // entry. Passing the entry along with defaulted attributes would present a
    // guess as a stat, so the page fails and the caller learns why.
    if let Some(status) = entry.attributes.rdattr_error {
        if !status.is_ok() {
            return Err(UmbraError::new(
                ErrorKind::Io,
                "list",
                format!(
                    "the server reported rdattr_error {} for entry {:?}",
                    status.0,
                    String::from_utf8_lossy(entry.name.as_bytes())
                ),
            ));
        }
    }
    let identity = identity_of(&entry.attributes).map_err(|error| error.to_umbra("list"))?;
    Ok(DirectoryEntry {
        name: umbra_core::BytePath::new(entry.name.as_bytes().to_vec())?,
        stat: blob_stat(identity, &entry.attributes)?,
    })
}

fn invalidated(context: &str) -> UmbraError {
    UmbraError::new(ErrorKind::StaleHandle, "list", context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::transport::Deadline;

    fn deadline() -> Deadline {
        Deadline { millis: 5_000 }
    }

    #[test]
    fn the_filter_removes_protocol_noise_and_private_state_but_not_dotfiles() {
        let protocol = NoiseFilter::protocol_only();
        assert!(protocol.is_noise(b"."));
        assert!(protocol.is_noise(b".."));
        assert!(!protocol.is_noise(crate::layout::PRIVATE_DIR));
        // A dotfile under the run root is the tracee's data. Hiding it would be a
        // false answer about the directory's contents.
        assert!(!protocol.is_noise(b".gitignore"));
        assert!(!protocol.is_noise(b"ordinary"));

        let run = NoiseFilter::with_private_state();
        assert!(run.is_noise(b"."));
        assert!(run.is_noise(b".."));
        assert!(run.is_noise(crate::layout::PRIVATE_DIR));
        assert!(!run.is_noise(b".gitignore"));
        assert!(!run.is_noise(b"root"));
    }

    #[test]
    fn a_cursor_round_trips_and_refuses_another_directory() {
        let directory = crate::handle::ObjectIdentity {
            fsid: crate::transport::Fsid { major: 3, minor: 4 },
            fileid: 77,
        };
        let cursor = PageCursor::new(directory, DirCookie(9), DirVerifier([0xAB; 8]));
        let encoded = cursor.encode();
        let decoded = PageCursor::decode(&encoded, directory).expect("our own cursor");
        assert_eq!(decoded, cursor);
        assert_eq!(decoded.cookie(), DirCookie(9));
        assert_eq!(decoded.verifier(), DirVerifier([0xAB; 8]));
        assert_eq!(decoded.directory(), directory);

        let elsewhere = crate::handle::ObjectIdentity {
            fileid: 78,
            ..directory
        };
        let error = PageCursor::decode(&encoded, elsewhere).unwrap_err();
        assert_eq!(error.kind, umbra_core::ErrorKind::StaleHandle);
        assert!(PageCursor::decode(&ListCursor(b"forged".to_vec()), directory).is_err());
    }

    #[test]
    fn a_page_is_bounded_and_its_cursor_resumes_without_dropping_an_entry() {
        let (mut fake, layout) = fixture::server();
        for index in 0..25u8 {
            fake.insert_file(&layout.root, format!("f{index:02}").as_bytes(), vec![index]);
        }
        let root = crate::identity::PinnedObject::pin(&mut fake, layout.root, deadline())
            .expect("pin the root anchor");
        let filter = NoiseFilter::protocol_only();

        let mut seen = Vec::new();
        let mut cursor = None;
        let mut rounds = 0;
        loop {
            let listed = page(&mut fake, &root, cursor.as_ref(), 4, &filter, deadline())
                .expect("a bounded page");
            assert!(
                listed.entries.len() <= 4,
                "a page never exceeds the requested limit"
            );
            seen.extend(listed.entries.iter().map(|e| e.name.as_bytes().to_vec()));
            cursor = listed.next;
            rounds += 1;
            assert!(rounds < 40, "paging must terminate");
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), 25, "every entry is delivered exactly once");
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 25, "no entry is delivered twice");
        let mut expected: Vec<Vec<u8>> = (0..25u8)
            .map(|index| format!("f{index:02}").into_bytes())
            .collect();
        expected.sort();
        assert_eq!(unique, expected);
    }

    #[test]
    fn no_whole_directory_is_materialised_for_one_page() {
        let (mut fake, layout) = fixture::server();
        for index in 0..500u32 {
            fake.insert_file(&layout.root, format!("f{index:04}").as_bytes(), Vec::new());
        }
        let root =
            crate::identity::PinnedObject::pin(&mut fake, layout.root, deadline()).expect("pin");
        let listed = page(
            &mut fake,
            &root,
            None,
            3,
            &NoiseFilter::protocol_only(),
            deadline(),
        )
        .expect("a bounded page");
        assert_eq!(listed.entries.len(), 3);
        assert!(
            listed.next.is_some(),
            "an unexhausted directory yields a continuation"
        );
    }

    #[test]
    fn a_rotated_cookie_verifier_invalidates_an_outstanding_cursor() {
        let (mut fake, layout) = fixture::server();
        for index in 0..10u8 {
            fake.insert_file(&layout.root, format!("f{index}").as_bytes(), Vec::new());
        }
        let root =
            crate::identity::PinnedObject::pin(&mut fake, layout.root, deadline()).expect("pin");
        let filter = NoiseFilter::protocol_only();
        let first = page(&mut fake, &root, None, 2, &filter, deadline()).expect("first page");
        let cursor = first.next.expect("more entries remain");

        // A server restart rotates the verifier, which voids every outstanding
        // cookie. Reporting that explicitly is the contract; silently restarting
        // from the beginning, or returning a cached snapshot, is not.
        fake.rotate_dir_verifier(DirVerifier([0x5A; 8]));
        let error = page(&mut fake, &root, Some(&cursor), 2, &filter, deadline()).unwrap_err();
        assert_eq!(error.kind, umbra_core::ErrorKind::StaleHandle);

        // A fresh enumeration still works: invalidation voids cursors, not the
        // directory.
        assert!(page(&mut fake, &root, None, 2, &filter, deadline()).is_ok());
    }

    #[test]
    fn listing_something_that_is_not_a_directory_is_refused() {
        let (mut fake, layout) = fixture::server();
        let file = fake.insert_file(&layout.root, b"note", b"x".to_vec());
        let pin = crate::identity::PinnedObject::pin(&mut fake, file, deadline()).expect("pin");
        let error = page(
            &mut fake,
            &pin,
            None,
            4,
            &NoiseFilter::protocol_only(),
            deadline(),
        )
        .unwrap_err();
        assert_eq!(error.kind, umbra_core::ErrorKind::InvalidPath);
    }

    #[test]
    fn the_run_anchor_hides_provider_state_from_its_listing() {
        let (mut fake, layout) = fixture::server();
        let run = crate::identity::PinnedObject::pin(&mut fake, layout.run, deadline())
            .expect("pin the run anchor");
        let listed = page(
            &mut fake,
            &run,
            None,
            16,
            &NoiseFilter::with_private_state(),
            deadline(),
        )
        .expect("a page");
        let names: Vec<_> = listed
            .entries
            .iter()
            .map(|entry| entry.name.as_bytes().to_vec())
            .collect();
        assert!(names.contains(&b"root".to_vec()));
        assert!(names.contains(&b"control".to_vec()));
        assert!(
            !names.contains(&crate::layout::PRIVATE_DIR.to_vec()),
            "provider-private state is not run content"
        );
    }
}
