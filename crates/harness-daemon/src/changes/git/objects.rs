//! Bounded local SHA-1 object reader; no alternate directories or lazy fetch.
use super::*;
use flate2::{Decompress, FlushDecompress, Status};
use std::{
    cell::Cell,
    io::{Seek, SeekFrom},
};
struct CountRead<R> {
    inner: R,
    count: Cell<usize>,
}
impl<R: Read> Read for &CountRead<R>
where
    for<'a> &'a R: Read,
{
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = (&self.inner).read(bytes)?;
        self.count.set(self.count.get() + n);
        Ok(n)
    }
}
pub(super) const MAX_OBJECT: usize = 4 * 1024 * 1024;
pub(super) const MAX_DELTA_DEPTH: usize = 16;
const MAX_PACKS: usize = 16;
const MAX_OBJECTS: usize = 1024;
struct Pack {
    name: String,
    index: Vec<u8>,
    count: usize,
}
struct Object {
    kind: u8,
    data: Vec<u8>,
}
struct Reader<'a> {
    dir: &'a Dir,
    budget: &'a mut Budget,
    packs: Option<Vec<Pack>>,
}
fn kind_name(kind: u8) -> R<&'static str> {
    match kind {
        1 => Ok("commit"),
        2 => Ok("tree"),
        3 => Ok("blob"),
        4 => Ok("tag"),
        _ => Err("git_object_type_unsupported"),
    }
}
fn object_hash(object: &Object) -> R<Oid> {
    let mut hash = Sha1::new();
    hash.update(format!(
        "{} {}\0",
        kind_name(object.kind)?,
        object.data.len()
    ));
    hash.update(&object.data);
    Ok(hash.finalize().into())
}
fn inflate(mut input: impl Read, expected: Option<usize>, budget: &mut Budget) -> R<Vec<u8>> {
    if expected.is_some_and(|n| n > MAX_OBJECT) {
        return Err("git_object_size_budget");
    }
    let max = expected.unwrap_or(MAX_OBJECT).min(budget.left);
    let mut decoder = Decompress::new(true);
    let mut result = Vec::new();
    let mut source = [0u8; 8192];
    let mut from = 0;
    let mut to = 0;
    let mut eof = false;
    loop {
        if from == to {
            to = input
                .read(&mut source)
                .map_err(|_| "git_object_unreadable")?;
            from = 0;
            eof = to == 0;
        }
        let mut output = [0u8; 8192];
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(
                &source[from..to],
                &mut output,
                if eof {
                    FlushDecompress::Finish
                } else {
                    FlushDecompress::None
                },
            )
            .map_err(|_| "git_object_invalid_compression")?;
        let used = (decoder.total_in() - before_in) as usize;
        let written = (decoder.total_out() - before_out) as usize;
        from += used;
        budget.charge(written)?;
        if result.len() + written > max {
            return Err("git_object_size_invalid");
        }
        result.extend_from_slice(&output[..written]);
        if status == Status::StreamEnd {
            break;
        }
        if used == 0 && written == 0 {
            return Err("git_object_invalid_compression");
        }
    }
    if expected.is_some_and(|n| n != result.len()) {
        return Err("git_object_size_invalid");
    }
    Ok(result)
}
impl Reader<'_> {
    fn charge_object(&mut self, depth: usize) -> R<()> {
        self.budget.objects += 1;
        if depth > MAX_DELTA_DEPTH || self.budget.objects > MAX_OBJECTS {
            return Err("git_object_depth_or_count_budget");
        }
        Ok(())
    }
    fn get(&mut self, oid: Oid, depth: usize) -> R<Object> {
        self.charge_object(depth)?;
        let text = hex(&oid);
        let path = format!("objects/{}/{}", &text[..2], &text[2..]);
        let loose = read(self.dir, &path, MAX_OBJECT + 1024, self.budget);
        if loose.is_err() && !missing(self.dir, &path) {
            return Err(loose.err().unwrap());
        }
        let object = if let Ok(compressed) = loose {
            let bytes = inflate(compressed.as_slice(), None, self.budget)?;
            let zero = bytes
                .iter()
                .position(|b| *b == 0)
                .ok_or("git_object_header_invalid")?;
            if zero > 64 {
                return Err("git_object_header_invalid");
            }
            let header =
                std::str::from_utf8(&bytes[..zero]).map_err(|_| "git_object_header_invalid")?;
            let (kind, size) = header.split_once(' ').ok_or("git_object_header_invalid")?;
            let size: usize = size.parse().map_err(|_| "git_object_header_invalid")?;
            if size != bytes.len() - zero - 1 {
                return Err("git_object_size_invalid");
            }
            let kind = match kind {
                "commit" => 1,
                "tree" => 2,
                "blob" => 3,
                "tag" => 4,
                _ => return Err("git_object_type_unsupported"),
            };
            Object {
                kind,
                data: bytes[zero + 1..].to_vec(),
            }
        } else {
            self.load_packs()?;
            let mut located = None;
            for pack in self.packs.as_ref().unwrap() {
                if let Some(offset) = pack_offset(pack, &oid)? {
                    located = Some((pack.name.clone(), offset));
                    break;
                }
            }
            let (name, offset) = located.ok_or("git_object_missing_or_unsupported")?;
            self.packed(&name, offset, depth)?
        };
        if object_hash(&object)? != oid {
            return Err("git_object_hash_mismatch");
        }
        Ok(object)
    }
    fn load_packs(&mut self) -> R<()> {
        if self.packs.is_some() {
            return Ok(());
        }
        let objects = self
            .dir
            .open_dir_nofollow("objects")
            .map_err(|_| "git_objects_unreadable")?;
        let packdir = match objects.open_dir_nofollow("pack") {
            Ok(d) => d,
            Err(_) => {
                self.packs = Some(Vec::new());
                return Ok(());
            }
        };
        let mut names = Vec::new();
        let mut visited = 0;
        for entry in packdir.entries().map_err(|_| "git_pack_unreadable")? {
            visited += 1;
            if visited > MAX_PACKS * 8 {
                return Err("git_pack_count_budget");
            }
            let entry = entry.map_err(|_| "git_pack_unreadable")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("pack-") && name.ends_with(".idx") {
                if names.len() >= MAX_PACKS
                    || !name.is_ascii()
                    || name.len() != 49
                    || !name[5..45].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err("git_pack_count_or_name_invalid");
                }
                names.push(name.to_owned());
            }
        }
        names.sort();
        let mut packs = Vec::new();
        for name in names {
            let bytes = read(&packdir, &name, MAX_META, self.budget)?;
            if bytes.len() < 8 + 1024 + 40 || &bytes[..4] != b"\xfftOc" || u32be(&bytes, 4)? != 2 {
                return Err("git_pack_index_version_unsupported");
            }
            let count = u32be(&bytes, 8 + 255 * 4)? as usize;
            if count > 200_000 || bytes.len() < 8 + 1024 + count * 28 + 40 {
                return Err("git_pack_index_budget_or_size");
            }
            let end = bytes.len() - 20;
            if Sha1::digest(&bytes[..end])[..] != bytes[end..] {
                return Err("git_pack_index_checksum");
            }
            let mut prev = 0;
            for slot in 0..256 {
                let value = u32be(&bytes, 8 + slot * 4)?;
                if value < prev || value as usize > count {
                    return Err("git_pack_index_invalid");
                }
                prev = value;
            }
            let names_start = 8 + 1024;
            let mut previous = None;
            for oid in bytes[names_start..names_start + count * 20]
                .as_chunks::<20>()
                .0
            {
                if previous.is_some_and(|p: &[u8; 20]| p >= oid) {
                    return Err("git_pack_index_order");
                }
                previous = Some(oid);
            }
            let large = bytes.len() - (8 + 1024 + count * 28 + 40);
            if !large.is_multiple_of(8) {
                return Err("git_pack_index_size");
            }
            packs.push(Pack {
                name: name.trim_end_matches(".idx").to_owned(),
                index: bytes,
                count,
            });
        }
        self.packs = Some(packs);
        Ok(())
    }
    fn packed(&mut self, name: &str, offset: u64, depth: usize) -> R<Object> {
        self.charge_object(depth)?;
        let mut opened = file(self.dir, &format!("objects/pack/{name}.pack"))?;
        let length = opened.metadata().map_err(|_| "git_pack_unreadable")?.len();
        if !(32..=4 * 1024 * 1024 * 1024).contains(&length) || offset < 12 || offset >= length - 20
        {
            return Err("git_pack_offset_or_size");
        }
        let mut header = [0; 12];
        opened
            .read_exact(&mut header)
            .map_err(|_| "git_pack_truncated")?;
        if &header[..4] != b"PACK"
            || ![2, 3].contains(&u32be(&header, 4)?)
            || u32be(&header, 8)? > 200_000
        {
            return Err("git_pack_header_invalid");
        }
        opened
            .seek(SeekFrom::Start(offset))
            .map_err(|_| "git_pack_unreadable")?;
        let max = 64;
        let mut bytes = Vec::new();
        (&opened)
            .take(max as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "git_pack_unreadable")?;
        self.budget.charge(bytes.len())?;
        let mut at = 0;
        let mut byte = take(&bytes, &mut at, 1)?[0];
        let kind = (byte >> 4) & 7;
        let mut size = (byte & 15) as usize;
        let mut shift = 4;
        while byte & 128 != 0 {
            if shift > 56 {
                return Err("git_integer_overflow");
            }
            byte = take(&bytes, &mut at, 1)?[0];
            size = size
                .checked_add(
                    ((byte & 127) as usize)
                        .checked_shl(shift)
                        .ok_or("git_integer_overflow")?,
                )
                .ok_or("git_integer_overflow")?;
            shift += 7;
        }
        if size > MAX_OBJECT {
            return Err("git_object_size_budget");
        }
        let base = match kind {
            6 => {
                let back = offset_varint(&bytes, &mut at)? as u64;
                if back == 0 {
                    return Err("git_delta_invalid_offset");
                }
                Some(self.packed(
                    name,
                    offset.checked_sub(back).ok_or("git_delta_invalid_offset")?,
                    depth + 1,
                )?)
            }
            7 => {
                let oid = take(&bytes, &mut at, 20)?.try_into().unwrap();
                Some(self.get(oid, depth + 1)?)
            }
            1..=4 => None,
            _ => return Err("git_object_type_unsupported"),
        };
        opened
            .seek(SeekFrom::Start(offset + at as u64))
            .map_err(|_| "git_pack_unreadable")?;
        let compressed_limit = MAX_OBJECT + 1024;
        if size + compressed_limit > self.budget.left {
            return Err("git_metadata_budget_exceeded");
        }
        let counted = CountRead {
            inner: opened,
            count: Cell::new(0),
        };
        let data = inflate(
            (&counted).take(compressed_limit as u64),
            Some(size),
            self.budget,
        )?;
        self.budget.charge(counted.count.get())?;
        match base {
            Some(base) => Ok(Object {
                kind: base.kind,
                data: apply_delta(&base.data, &data, self.budget)?,
            }),
            None => Ok(Object { kind, data }),
        }
    }
}
fn pack_offset(pack: &Pack, oid: &Oid) -> R<Option<u64>> {
    let start = 8 + 1024;
    let names = &pack.index[start..start + pack.count * 20];
    let mut low = 0;
    let mut high = pack.count;
    while low < high {
        let mid = low + (high - low) / 2;
        match names[mid * 20..mid * 20 + 20].cmp(oid) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
            std::cmp::Ordering::Equal => {
                let off = u32be(&pack.index, start + pack.count * 24 + mid * 4)?;
                if off & 0x8000_0000 == 0 {
                    return Ok(Some(u64::from(off)));
                }
                let at = start + pack.count * 28 + (off & 0x7fff_ffff) as usize * 8;
                if at + 8 > pack.index.len() - 40 {
                    return Err("git_pack_large_offset_invalid");
                }
                return Ok(Some(u64::from_be_bytes(
                    pack.index[at..at + 8].try_into().unwrap(),
                )));
            }
        }
    }
    Ok(None)
}
fn size_varint(data: &[u8], at: &mut usize) -> R<usize> {
    let mut size = 0usize;
    for shift in (0..=56).step_by(7) {
        let byte = take(data, at, 1)?[0];
        size = size
            .checked_add(
                ((byte & 127) as usize)
                    .checked_shl(shift)
                    .ok_or("git_integer_overflow")?,
            )
            .ok_or("git_integer_overflow")?;
        if byte & 128 == 0 {
            return Ok(size);
        }
    }
    Err("git_integer_overflow")
}
fn apply_delta(base: &[u8], delta: &[u8], budget: &mut Budget) -> R<Vec<u8>> {
    let mut at = 0;
    let original = size_varint(delta, &mut at)?;
    let length = size_varint(delta, &mut at)?;
    if original != base.len() || length > MAX_OBJECT {
        return Err("git_delta_size_invalid");
    }
    budget.charge(length)?;
    let mut out = Vec::with_capacity(length);
    while at < delta.len() {
        let op = take(delta, &mut at, 1)?[0];
        if op & 128 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for bit in 0..4 {
                if op & (1 << bit) != 0 {
                    offset |= (take(delta, &mut at, 1)?[0] as usize) << (bit * 8);
                }
            }
            for bit in 0..3 {
                if op & (1 << (bit + 4)) != 0 {
                    size |= (take(delta, &mut at, 1)?[0] as usize) << (bit * 8);
                }
            }
            if size == 0 {
                size = 65536;
            }
            let end = offset.checked_add(size).ok_or("git_delta_range_invalid")?;
            if out.len().checked_add(size).is_none_or(|n| n > length) {
                return Err("git_delta_output_budget");
            }
            out.extend_from_slice(base.get(offset..end).ok_or("git_delta_range_invalid")?);
        } else if op != 0 {
            let count = op as usize;
            if out.len() + count > length {
                return Err("git_delta_output_budget");
            }
            out.extend_from_slice(take(delta, &mut at, count)?);
        } else {
            return Err("git_delta_instruction_invalid");
        }
    }
    if out.len() != length {
        return Err("git_delta_size_invalid");
    }
    Ok(out)
}
pub(super) fn tree(dir: &Dir, oid: Oid, budget: &mut Budget) -> R<BTreeMap<String, Entry>> {
    let mut reader = Reader {
        dir,
        budget,
        packs: None,
    };
    let commit = reader.get(oid, 0)?;
    if commit.kind != 1 {
        return Err("git_head_not_commit");
    }
    let first = commit
        .data
        .split(|b| *b == b'\n')
        .next()
        .ok_or("git_commit_invalid")?;
    let first = std::str::from_utf8(first).map_err(|_| "git_commit_invalid")?;
    let tree = oid_hex(first.strip_prefix("tree ").ok_or("git_commit_invalid")?)?;
    let mut result = BTreeMap::new();
    walk(&mut reader, tree, "", 0, &mut result)?;
    Ok(result)
}
fn walk(
    reader: &mut Reader<'_>,
    oid: Oid,
    prefix: &str,
    depth: usize,
    out: &mut BTreeMap<String, Entry>,
) -> R<()> {
    if depth > 24 {
        return Err("git_tree_depth_budget");
    }
    let object = reader.get(oid, 0)?;
    if object.kind != 2 {
        return Err("git_tree_type_invalid");
    }
    let mut at = 0;
    let mut seen = BTreeSet::new();
    let mut previous_key: Option<Vec<u8>> = None;
    while at < object.data.len() {
        if out.len() >= MAX_ENTRIES {
            return Err("git_tree_entry_budget");
        }
        let rest = &object.data[at..];
        let space = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or("git_tree_invalid")?;
        if space > 6 {
            return Err("git_tree_invalid");
        }
        let mode = u32::from_str_radix(
            std::str::from_utf8(take(&object.data, &mut at, space)?)
                .map_err(|_| "git_tree_invalid")?,
            8,
        )
        .map_err(|_| "git_tree_invalid")?;
        take(&object.data, &mut at, 1)?;
        let rest = &object.data[at..];
        let zero = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or("git_tree_invalid")?;
        let name = std::str::from_utf8(take(&object.data, &mut at, zero)?)
            .map_err(|_| "git_path_encoding_unsupported")?;
        if !safe_path(name) || name.contains('/') || name.eq_ignore_ascii_case(".git") {
            return Err("git_tree_path_invalid");
        }
        if !seen.insert(name.to_owned()) {
            return Err("git_tree_duplicate_name");
        }
        let mut key = name.as_bytes().to_vec();
        key.push(if mode == 0o40000 { b'/' } else { 0 });
        if previous_key
            .as_ref()
            .is_some_and(|previous| previous >= &key)
        {
            return Err("git_tree_order_invalid");
        }
        previous_key = Some(key);
        take(&object.data, &mut at, 1)?;
        let oid = take(&object.data, &mut at, 20)?.try_into().unwrap();
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if path.len() > MAX_PATH {
            return Err("git_tree_path_budget");
        }
        reader.budget.charge(path.len() + 64)?;
        if mode == 0o40000 {
            walk(reader, oid, &path, depth + 1, out)?;
        } else {
            if ![0o100644, 0o100755, 0o120000, 0o160000].contains(&mode)
                || out
                    .insert(
                        path,
                        Entry {
                            oid,
                            mode,
                            stage: 0,
                            flags: 0,
                        },
                    )
                    .is_some()
            {
                return Err("git_tree_entry_invalid");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    fn compressed(bytes: &[u8]) -> Vec<u8> {
        let mut writer =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap()
    }
    #[test]
    fn zlib_requires_actual_stream_end_and_obeys_size_budget() {
        let bytes = compressed(&vec![b'x'; 40_000]);
        assert_eq!(
            inflate(bytes.as_slice(), Some(40_000), &mut Budget::new())
                .unwrap()
                .len(),
            40_000
        );
        for n in 0..bytes.len() {
            assert!(
                inflate(&bytes[..n], Some(40_000), &mut Budget::new()).is_err(),
                "truncated at {n}"
            );
        }
        assert!(inflate(bytes.as_slice(), Some(39_999), &mut Budget::new()).is_err());
        assert!(inflate(
            compressed(&vec![0; MAX_OBJECT + 1]).as_slice(),
            None,
            &mut Budget::new()
        )
        .is_err());
        let mut budget = Budget {
            left: 20,
            objects: 0,
        };
        assert!(inflate(bytes.as_slice(), None, &mut budget).is_err());
    }
    #[test]
    fn deltas_validate_copy_ranges_output_size_and_reserved_opcodes() {
        assert_eq!(
            apply_delta(b"abc", &[3, 5, 0x90, 3, 2, b'Z', b'Z'], &mut Budget::new()).unwrap(),
            b"abcZZ"
        );
        for delta in [
            &[3, 3, 0][..],
            &[3, 4, 0x91, 2, 4][..],
            &[2, 3, 0x90, 3][..],
            &[3, 1, 2, b'a', b'b'][..],
            &[3, 5, 1, b'a'][..],
        ] {
            assert!(apply_delta(b"abc", delta, &mut Budget::new()).is_err());
        }
        assert!(size_varint(&[255; 10], &mut 0).is_err());
        assert!(offset_varint(&[255; 10], &mut 0).is_err());
    }

    #[test]
    fn trees_reject_duplicate_names_file_directory_collisions_and_unsorted_entries() {
        let root =
            std::env::temp_dir().join(format!("git-tree-fixture-{}", harness_core::new_id()));
        std::fs::create_dir_all(root.join("objects")).unwrap();
        fn object(root: &Path, bytes: &[u8]) -> Oid {
            let mut data = format!("tree {}\0", bytes.len()).into_bytes();
            data.extend_from_slice(bytes);
            let oid: Oid = Sha1::digest(&data).into();
            let text = hex(&oid);
            let path = root.join("objects").join(&text[..2]).join(&text[2..]);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, compressed(&data)).unwrap();
            oid
        }
        fn entry(mode: &str, name: &str, oid: Oid) -> Vec<u8> {
            let mut bytes = format!("{mode} {name}\0").into_bytes();
            bytes.extend_from_slice(&oid);
            bytes
        }
        let empty = object(&root, b"");
        for bytes in [
            [
                entry("100644", "same", [1; 20]),
                entry("100644", "same", [2; 20]),
            ]
            .concat(),
            [
                entry("100644", "same", [1; 20]),
                entry("40000", "same", empty),
            ]
            .concat(),
            [entry("40000", "same", empty), entry("40000", "same", empty)].concat(),
            [entry("100644", "z", [1; 20]), entry("100644", "a", [2; 20])].concat(),
        ] {
            let oid = object(&root, &bytes);
            let dir = Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
            let mut budget = Budget::new();
            let mut reader = Reader {
                dir: &dir,
                budget: &mut budget,
                packs: None,
            };
            assert!(walk(&mut reader, oid, "", 0, &mut BTreeMap::new()).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
