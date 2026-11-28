//! Bounded ISO-BMFF / QuickTime movie checks around XMP Toolkit rewrites.
//!
//! The toolkit owns the movie's XMP packet: `moov/udta/XMP_` in QuickTime
//! files and a top-level XMP `uuid` box in ISO MP4 files. When the new `moov`
//! does not fit, it writes the new `moov` at the end of the file and turns the
//! old one into `free`. In QuickTime files it also owns the `Cr8r` and `cprt`
//! user-data atoms. Every other box must survive unchanged, and top-level boxes
//! at their original offsets, so media data and the chunk offset tables that
//! point into it stay valid.

use crate::download::file::ExistingFileFingerprint;
use anyhow::{Context, Result, bail, ensure};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// XMP packet wrapper UUID for ISO base media files (XMP Specification Part 3).
const XMP_UUID: [u8; 16] = [
    0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac,
];
const BOX_BUDGET: usize = 100_000;
const MAX_DEPTH: usize = 32;
const COMPARE_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
struct MovieBox {
    kind: [u8; 4],
    start: u64,
    body: u64,
    end: u64,
}

impl MovieBox {
    fn len(self) -> u64 {
        self.end - self.start
    }
}

fn read_box(file: &mut File, offset: u64, end: u64) -> Result<MovieBox> {
    ensure!(end - offset >= 8, "Incomplete movie box header");
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 8];
    file.read_exact(&mut header)?;
    let [s0, s1, s2, s3, k0, k1, k2, k3] = header;
    let kind = [k0, k1, k2, k3];
    let (size, header_size) = match u32::from_be_bytes([s0, s1, s2, s3]) {
        0 => (end - offset, 8),
        1 => {
            ensure!(end - offset >= 16, "Incomplete extended movie box header");
            let mut extended = [0; 8];
            file.read_exact(&mut extended)?;
            (u64::from_be_bytes(extended), 16)
        }
        size => (u64::from(size), 8),
    };
    ensure!(
        size >= header_size && size <= end - offset,
        "Invalid movie box bounds"
    );
    Ok(MovieBox {
        kind,
        start: offset,
        body: offset + header_size,
        end: offset + size,
    })
}

/// Whether the XMP Toolkit's MPEG-4 handler can open the file as a movie.
///
/// Mirrors `MPEG4_CheckFormat`: an `ftyp` first box, or classic QuickTime
/// top-level boxes up to `moov`. Like the toolkit, any box whose type is
/// printable ASCII or 0xA9 is tolerated before `moov`, which covers `pnot`
/// previews and stray user-data boxes. Images never match: their first eight
/// bytes do not form a box that fits in the file with a printable type. A
/// file whose first box names a movie type but has broken bounds counts as a
/// movie, so the layout check refuses it instead of skipping it.
fn is_movie(file: &mut File, len: u64) -> Result<bool> {
    let mut offset = 0;
    let mut budget = BOX_BUDGET;
    while offset < len && budget > 0 {
        budget -= 1;
        let Ok(movie_box) = read_box(file, offset, len) else {
            if offset == 0 && len >= 8 {
                file.seek(SeekFrom::Start(0))?;
                let mut header = [0; 8];
                file.read_exact(&mut header)?;
                let [_, _, _, _, k0, k1, k2, k3] = header;
                return Ok(matches!(
                    &[k0, k1, k2, k3],
                    b"ftyp" | b"moov" | b"mdat" | b"pnot" | b"wide" | b"free" | b"skip"
                ));
            }
            return Ok(false);
        };
        if &movie_box.kind == b"moov" || (offset == 0 && &movie_box.kind == b"ftyp") {
            return Ok(true);
        }
        if !movie_box
            .kind
            .iter()
            .all(|&byte| (0x20..=0x7e).contains(&byte) || byte == 0xa9)
        {
            return Ok(false);
        }
        offset = movie_box.end;
    }
    Ok(false)
}

/// Whether the toolkit opens `path` as a movie; see [`is_movie`].
pub(super) fn is_movie_file(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    is_movie(&mut file, len)
}

/// Bytes to skip before a `meta` box's children: 4 for an ISO FullBox, 0 for
/// a bare QuickTime atom. Both forms put `hdlr` first, so its position decides;
/// without one, a nested all-zero prefix is taken as a FullBox header.
fn meta_prefix_len(file: &mut File, movie_box: MovieBox, parent: Option<&[u8; 4]>) -> Result<u64> {
    let available = movie_box.end - movie_box.body;
    if available < 4 {
        ensure!(parent == Some(b"moov"), "Incomplete movie metadata prefix");
        return Ok(0);
    }
    let mut head = [0; 12];
    let read = usize::try_from(available.min(12))?;
    file.seek(SeekFrom::Start(movie_box.body))?;
    file.read_exact(head.get_mut(..read).unwrap_or_default())?;
    let [p0, p1, p2, p3, a0, a1, a2, a3, b0, b1, b2, b3] = head;
    if read >= 8 && &[a0, a1, a2, a3] == b"hdlr" {
        return Ok(0);
    }
    if read >= 12 && &[b0, b1, b2, b3] == b"hdlr" {
        return Ok(4);
    }
    Ok(if parent != Some(b"moov") && [p0, p1, p2, p3] == [0; 4] {
        4
    } else {
        0
    })
}

/// Rejects malformed movie box bounds before the toolkit opens the file. A
/// nested `meta` may be an ISO FullBox or a bare QuickTime atom.
pub(super) fn validate_movie_metadata_layout(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let end = file.metadata()?.len();
    if !is_movie(&mut file, end)? {
        return Ok(());
    }
    let mut budget = BOX_BUDGET;
    validate_movie_boxes(&mut file, 0, end, None, 0, &mut budget)
}

fn validate_movie_boxes(
    file: &mut File,
    mut offset: u64,
    end: u64,
    parent: Option<&[u8; 4]>,
    depth: usize,
    budget: &mut usize,
) -> Result<()> {
    ensure!(
        depth <= MAX_DEPTH,
        "Movie metadata nesting exceeds preservation check limit"
    );
    while offset < end {
        if udta_terminator(file, offset, end, parent)? {
            return Ok(());
        }
        ensure!(
            *budget > 0,
            "Movie box count exceeds preservation check limit"
        );
        *budget -= 1;
        let movie_box = read_box(file, offset, end)?;
        let kind = &movie_box.kind;
        let mut children = movie_box.body;
        if kind == b"moov"
            || (parent.is_some()
                && matches!(
                    kind,
                    b"udta"
                        | b"meta"
                        | b"ilst"
                        | b"trak"
                        | b"edts"
                        | b"mdia"
                        | b"minf"
                        | b"dinf"
                        | b"stbl"
                ))
        {
            if kind == b"meta" {
                children += meta_prefix_len(file, movie_box, parent)?;
            }
            validate_movie_boxes(file, children, movie_box.end, Some(kind), depth + 1, budget)?;
        }
        offset = movie_box.end;
    }
    Ok(())
}

/// QuickTime user-data lists may end with a four-byte zero terminator.
fn udta_terminator(
    file: &mut File,
    offset: u64,
    end: u64,
    parent: Option<&[u8; 4]>,
) -> Result<bool> {
    if parent != Some(b"udta") || end - offset != 4 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut terminator = [0; 4];
    file.read_exact(&mut terminator)?;
    ensure!(terminator == [0; 4], "Invalid movie user-data terminator");
    Ok(true)
}

fn children(
    file: &mut File,
    mut offset: u64,
    end: u64,
    parent: Option<&[u8; 4]>,
    budget: &mut usize,
) -> Result<Vec<MovieBox>> {
    let mut boxes = Vec::new();
    while offset < end {
        if udta_terminator(file, offset, end, parent)? {
            break;
        }
        ensure!(
            *budget > 0,
            "Movie box count exceeds preservation check limit"
        );
        *budget -= 1;
        let movie_box = read_box(file, offset, end)?;
        offset = movie_box.end;
        boxes.push(movie_box);
    }
    Ok(boxes)
}

fn is_padding(kind: &[u8; 4]) -> bool {
    matches!(kind, b"free" | b"skip" | b"wide")
}

fn is_xmp_uuid(file: &mut File, movie_box: MovieBox) -> Result<bool> {
    if &movie_box.kind != b"uuid" || movie_box.end - movie_box.body < 16 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(movie_box.body))?;
    let mut uuid = [0; 16];
    file.read_exact(&mut uuid)?;
    Ok(uuid == XMP_UUID)
}

/// Refuses a toolkit rewrite that changed anything outside the movie's XMP
/// packet, padding and toolkit-owned user data, or that moved a protected box.
///
/// Returns the rewritten file's fingerprint for movies, computed in the same
/// pass that compares the media, so callers need not read the file again.
/// Returns `None` for files the toolkit does not open as movies.
pub(super) fn validate_movie_rewrite_preserves_media(
    original: &Path,
    rewritten: &Path,
) -> Result<Option<ExistingFileFingerprint>> {
    // CONTRACT: MOVIE_EMBED_REWRITE_PRESERVES_MEDIA
    let mut before = File::open(original)
        .with_context(|| format!("Could not open {} for comparison", original.display()))?;
    let before_len = before.metadata()?.len();
    if !is_movie(&mut before, before_len)? {
        return Ok(None);
    }
    let mut after = File::open(rewritten)
        .with_context(|| format!("Could not open {} for comparison", rewritten.display()))?;
    let after_len = after.metadata()?.len();
    let mut before_budget = BOX_BUDGET;
    let mut after_budget = BOX_BUDGET;
    let old_top = children(&mut before, 0, before_len, None, &mut before_budget)?;
    let new_top = children(&mut after, 0, after_len, None, &mut after_budget)?;

    let old = split_top_level(&mut before, &old_top)?;
    let new = split_top_level(&mut after, &new_top)?;
    ensure!(
        old.protected.len() == new.protected.len(),
        "Refusing embedded metadata rewrite: the XMP toolkit added or removed a top-level movie box"
    );
    for (old_box, new_box) in old.protected.iter().zip(&new.protected) {
        ensure!(
            old_box.kind == new_box.kind
                && old_box.start == new_box.start
                && old_box.len() == new_box.len(),
            "Refusing embedded metadata rewrite: the XMP toolkit moved or resized a top-level movie box"
        );
    }
    let (Some(old_moov), Some(new_moov)) = (old.moov, new.moov) else {
        bail!("Refusing embedded metadata rewrite: the movie must keep exactly one `moov` box");
    };
    let mut buffers = CompareBuffers::new();
    let old_moov = children(
        &mut before,
        old_moov.body,
        old_moov.end,
        Some(b"moov"),
        &mut before_budget,
    )?;
    let new_moov = children(
        &mut after,
        new_moov.body,
        new_moov.end,
        Some(b"moov"),
        &mut after_budget,
    )?;
    let old_moov = split_movie(&mut before, &old_moov, &mut before_budget)?;
    let new_moov = split_movie(&mut after, &new_moov, &mut after_budget)?;
    require_same_boxes(
        &mut buffers,
        &mut before,
        &old_moov.protected,
        &mut after,
        &new_moov.protected,
        "Refusing embedded metadata rewrite: the XMP toolkit changed movie tracks or metadata",
    )?;
    require_same_boxes(
        &mut buffers,
        &mut before,
        &old_moov.user_data,
        &mut after,
        &new_moov.user_data,
        "Refusing embedded metadata rewrite: the XMP toolkit changed movie user data",
    )?;
    // Last, because it reads the whole file: the protected top-level boxes
    // (ftyp, mdat, ...) sit at the same offsets in both files.
    buffers
        .hash_comparing(
            &mut after,
            &mut before,
            &new.protected,
            "Refusing embedded metadata rewrite: the XMP toolkit changed a top-level movie box",
        )
        .map(Some)
}

struct TopLevel {
    moov: Option<MovieBox>,
    /// Boxes that must keep their offset, size and bytes.
    protected: Vec<MovieBox>,
}

fn split_top_level(file: &mut File, boxes: &[MovieBox]) -> Result<TopLevel> {
    let mut moov = None;
    let mut protected = Vec::new();
    for movie_box in boxes {
        if &movie_box.kind == b"moov" {
            ensure!(
                moov.replace(*movie_box).is_none(),
                "Refusing embedded metadata rewrite: the movie must keep exactly one `moov` box"
            );
        } else if !is_padding(&movie_box.kind) && !is_xmp_uuid(file, *movie_box)? {
            protected.push(*movie_box);
        }
    }
    Ok(TopLevel { moov, protected })
}

struct MovieChildren {
    /// `udta` children the toolkit does not own, in file order.
    user_data: Vec<MovieBox>,
    /// Every other `moov` child (header, tracks, QuickTime metadata), in order.
    protected: Vec<MovieBox>,
}

/// `udta` children the toolkit owns: the XMP packet, the `Cr8r` creator atom
/// it derives from `xmp:CreatorTool` in QuickTime files, and the `cprt`
/// copyrights it reconciles with `dc:rights`.
fn is_toolkit_user_data(kind: &[u8; 4]) -> bool {
    matches!(kind, b"XMP_" | b"Cr8r" | b"cprt") || is_padding(kind)
}

fn split_movie(file: &mut File, boxes: &[MovieBox], budget: &mut usize) -> Result<MovieChildren> {
    let mut split = MovieChildren {
        user_data: Vec::new(),
        protected: Vec::new(),
    };
    for movie_box in boxes {
        match &movie_box.kind {
            b"udta" => {
                for child in children(file, movie_box.body, movie_box.end, Some(b"udta"), budget)? {
                    if !is_toolkit_user_data(&child.kind) {
                        split.user_data.push(child);
                    }
                }
            }
            kind if is_padding(kind) => {}
            _ => split.protected.push(*movie_box),
        }
    }
    Ok(split)
}

fn require_same_boxes(
    buffers: &mut CompareBuffers,
    before: &mut File,
    old: &[MovieBox],
    after: &mut File,
    new: &[MovieBox],
    reason: &'static str,
) -> Result<()> {
    ensure!(old.len() == new.len(), "{reason}");
    for (old_box, new_box) in old.iter().zip(new) {
        ensure!(
            old_box.kind == new_box.kind && old_box.len() == new_box.len(),
            "{reason}"
        );
        buffers.require_equal(before, *old_box, after, *new_box, reason)?;
    }
    Ok(())
}

struct CompareBuffers {
    left: Vec<u8>,
    right: Vec<u8>,
}

impl CompareBuffers {
    fn new() -> Self {
        Self {
            left: vec![0; COMPARE_BUFFER_BYTES],
            right: vec![0; COMPARE_BUFFER_BYTES],
        }
    }

    fn require_equal(
        &mut self,
        before: &mut File,
        old: MovieBox,
        after: &mut File,
        new: MovieBox,
        reason: &'static str,
    ) -> Result<()> {
        ensure!(old.len() == new.len(), "{reason}");
        before.seek(SeekFrom::Start(old.start))?;
        after.seek(SeekFrom::Start(new.start))?;
        let mut remaining = old.len();
        while remaining > 0 {
            let count = usize::try_from(remaining.min(COMPARE_BUFFER_BYTES as u64))?;
            let (Some(left), Some(right)) =
                (self.left.get_mut(..count), self.right.get_mut(..count))
            else {
                bail!("Movie comparison buffer is too small");
            };
            before.read_exact(left)?;
            after.read_exact(right)?;
            ensure!(left == right, "{reason}");
            remaining -= count as u64;
        }
        Ok(())
    }

    /// Hash all of `after` in one sequential read, comparing every byte inside
    /// a `protected` range with the byte at the same offset in `before`.
    /// `protected` is in file order and its ranges do not overlap.
    fn hash_comparing(
        &mut self,
        after: &mut File,
        before: &mut File,
        protected: &[MovieBox],
        reason: &'static str,
    ) -> Result<ExistingFileFingerprint> {
        use sha2::{Digest, Sha256};
        let Self { left, right } = self;
        let mut sha256 = Sha256::new();
        after.seek(SeekFrom::Start(0))?;
        let mut position = 0_u64;
        let mut next = 0;
        loop {
            let read = after.read(right)?;
            if read == 0 {
                break;
            }
            let chunk = right.get(..read).unwrap_or_default();
            sha256.update(chunk);
            let end = position + read as u64;
            while let Some(range) = protected.get(next) {
                if range.start >= end {
                    break;
                }
                let from = range.start.max(position);
                let to = range.end.min(end);
                let offset = usize::try_from(from - position)?;
                let len = usize::try_from(to - from)?;
                let (Some(expected), Some(actual)) =
                    (left.get_mut(..len), chunk.get(offset..offset + len))
                else {
                    bail!("Movie comparison buffer is too small");
                };
                before.seek(SeekFrom::Start(from))?;
                before.read_exact(expected)?;
                ensure!(expected == actual, "{reason}");
                if range.end > end {
                    break;
                }
                next += 1;
            }
            position = end;
        }
        Ok(ExistingFileFingerprint {
            size: position,
            sha256: sha256.finalize().into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{XMP_UUID, validate_movie_rewrite_preserves_media};
    use std::fs;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    fn mvhd(version: u8, created: u64, modified: u64, timescale: u32) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        if version == 1 {
            body.extend(created.to_be_bytes());
            body.extend(modified.to_be_bytes());
            body.extend(timescale.to_be_bytes());
            body.extend([0; 8]);
        } else {
            body.extend(u32::try_from(created).unwrap().to_be_bytes());
            body.extend(u32::try_from(modified).unwrap().to_be_bytes());
            body.extend(timescale.to_be_bytes());
            body.extend([0; 4]);
        }
        body.extend([0; 80]);
        atom(b"mvhd", &body)
    }

    fn moov(header: &[u8], udta: &[u8]) -> Vec<u8> {
        let mut body = header.to_vec();
        body.extend(atom(b"trak", &atom(b"tkhd", &[0; 84])));
        body.extend(atom(b"udta", udta));
        atom(b"moov", &body)
    }

    fn movie(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    fn check(before: &[u8], after: &[u8]) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (original, rewritten) = (dir.path().join("a.mov"), dir.path().join("b.mov"));
        fs::write(&original, before)?;
        fs::write(&rewritten, after)?;
        if let Some(fingerprint) = validate_movie_rewrite_preserves_media(&original, &rewritten)? {
            // The comparison pass doubles as the rewritten file's fingerprint.
            anyhow::ensure!(
                fingerprint == crate::download::metadata::prepared::fingerprint_bytes(after)?,
                "fingerprint differs from the rewritten bytes"
            );
        }
        Ok(())
    }

    fn refused(before: &[u8], after: &[u8]) -> String {
        let error = check(before, after).unwrap_err().to_string();
        assert!(
            error.starts_with("Refusing embedded metadata rewrite"),
            "{error}"
        );
        error
    }

    fn ftyp() -> Vec<u8> {
        atom(b"ftyp", b"qt  \0\0\0\0qt  ")
    }

    fn mdat() -> Vec<u8> {
        atom(b"mdat", b"media payload")
    }

    fn native_name() -> Vec<u8> {
        atom(b"name", b"kept")
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_accepts_relocated_quicktime_moov() {
        let old_moov = moov(&mvhd(0, 5, 6, 600), &native_name());
        let before = movie(&[ftyp(), old_moov.clone(), mdat()]);
        let mut udta = native_name();
        udta.extend(atom(b"XMP_", b"<x:xmpmeta/>"));
        // Toolkit-owned creator and copyright atoms may appear or change.
        udta.extend(atom(b"Cr8r", &[0; 16]));
        udta.extend(atom(b"cprt", b"\0\0\0\0\x15\xc7(c)\0"));
        let after = movie(&[
            ftyp(),
            atom(b"free", &vec![0; old_moov.len() - 8]),
            mdat(),
            moov(&mvhd(0, 5, 6, 600), &udta),
        ]);
        check(&before, &after).unwrap();
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_accepts_mp4_uuid_packet() {
        let before = movie(&[ftyp(), mdat(), moov(&mvhd(1, 0, 0, 600), &native_name())]);
        let mut packet = XMP_UUID.to_vec();
        packet.extend_from_slice(b"<x:xmpmeta/>");
        let after = movie(&[
            ftyp(),
            mdat(),
            moov(&mvhd(1, 0, 0, 600), &native_name()),
            atom(b"uuid", &packet),
        ]);
        check(&before, &after).unwrap();
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_refuses_moved_media() {
        let before = movie(&[ftyp(), moov(&mvhd(0, 0, 0, 600), &[]), mdat()]);
        let after = movie(&[
            ftyp(),
            atom(b"free", &[]),
            moov(&mvhd(0, 0, 0, 600), &[]),
            mdat(),
        ]);
        assert!(refused(&before, &after).contains("moved or resized"));
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_refuses_changed_media_and_tracks() {
        let header = mvhd(0, 0, 0, 600);
        let before = movie(&[ftyp(), mdat(), moov(&header, &[])]);
        let changed_media = movie(&[ftyp(), atom(b"mdat", b"media PAYLOAD"), moov(&header, &[])]);
        assert!(refused(&before, &changed_media).contains("top-level movie box"));

        let mut track = header.clone();
        track.extend(atom(b"trak", &atom(b"tkhd", &[1; 84])));
        track.extend(atom(b"udta", &[]));
        let changed_track = movie(&[ftyp(), mdat(), atom(b"moov", &track)]);
        assert!(refused(&before, &changed_track).contains("tracks or metadata"));
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_refuses_any_header_change() {
        // Native creation and modification times are capture data too.
        for version in [0, 1] {
            let before = movie(&[ftyp(), mdat(), moov(&mvhd(version, 1, 2, 600), &[])]);
            for after in [
                mvhd(version, 9, 2, 600),
                mvhd(version, 1, 9, 600),
                mvhd(version, 1, 2, 601),
            ] {
                let after = movie(&[ftyp(), mdat(), moov(&after, &[])]);
                assert!(refused(&before, &after).contains("tracks or metadata"));
            }
        }
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_checks_classic_quicktime_starts() {
        // The toolkit opens files that start with a preview or any printable
        // box before `moov`; so does the check.
        for lead in [atom(b"pnot", &[0; 12]), atom(b"udta", &native_name())] {
            let before = movie(&[lead.clone(), moov(&mvhd(0, 0, 0, 600), &[]), mdat()]);
            let after = movie(&[
                lead,
                moov(&mvhd(0, 0, 0, 600), &[]),
                atom(b"mdat", b"media PAYLOAD"),
            ]);
            assert!(refused(&before, &after).contains("top-level movie box"));
        }
    }

    #[test]
    fn movie_layout_check_tells_bare_and_full_meta_apart() {
        let dir = tempfile::tempdir().unwrap();
        let hdlr = atom(b"hdlr", &[0; 24]);
        let full_with_flags = [vec![0, 0, 0, 1], hdlr.clone()].concat();
        for body in [
            hdlr.clone(),
            [vec![0; 4], hdlr.clone()].concat(),
            full_with_flags,
        ] {
            let track = atom(b"trak", &atom(b"meta", &body));
            let bytes = movie(&[
                ftyp(),
                atom(b"moov", &[mvhd(0, 0, 0, 600), track].concat()),
                mdat(),
            ]);
            let path = dir.path().join("meta.mov");
            fs::write(&path, bytes).unwrap();
            super::validate_movie_metadata_layout(&path).unwrap();
        }
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_refuses_changed_user_data() {
        let before = movie(&[ftyp(), mdat(), moov(&mvhd(0, 0, 0, 600), &native_name())]);
        let lost = movie(&[ftyp(), mdat(), moov(&mvhd(0, 0, 0, 600), &[])]);
        assert!(refused(&before, &lost).contains("user data"));
        let changed = movie(&[
            ftyp(),
            mdat(),
            moov(&mvhd(0, 0, 0, 600), &atom(b"name", b"lost")),
        ]);
        assert!(refused(&before, &changed).contains("user data"));
    }

    #[test]
    fn contract_movie_embed_rewrite_preserves_media_refuses_added_box_or_second_moov() {
        let moov_box = moov(&mvhd(0, 0, 0, 600), &[]);
        let before = movie(&[ftyp(), mdat(), moov_box.clone()]);
        let added = movie(&[ftyp(), mdat(), moov_box.clone(), atom(b"junk", b"x")]);
        assert!(refused(&before, &added).contains("added or removed"));
        let second = movie(&[ftyp(), mdat(), moov_box.clone(), moov_box]);
        assert!(refused(&before, &second).contains("exactly one `moov`"));
        let truncated = movie(&[ftyp(), mdat(), b"\0\0\0\x40moov".to_vec()]);
        assert!(check(&before, &truncated).is_err());
    }

    #[test]
    fn movie_rewrite_check_ignores_non_movies() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0, 0x10, b'J', b'F', b'I', b'F'];
        check(&jpeg, b"anything").unwrap();
    }
}
