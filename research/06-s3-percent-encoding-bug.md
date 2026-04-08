# S3 Client Bugs in s3-rootfs

## Summary

Two bugs in `s3-rootfs/src/s3.rs`, both in the LIST path:

1. **Missing URL-decode on list keys** — `parse_list_response()` does
   not URL-decode `<Key>` values from the XML response. Since rusty-s3
   requests `encoding-type=url`, keys containing `%` appear
   double-encoded (e.g., `%40` → `%2540`).

2. **No pagination** — `list()` fetches a single page (`max-keys=1000`)
   and never follows `NextContinuationToken`. Directories with >1000
   entries silently lose the rest.

Neither bug is in rusty-s3, ureq, or RustFS. Both are in our client
code.

## Context

systemd uses percent-encoding in actual filenames on disk. Template
instance symlinks in `.wants` directories have literal `%40` in the
filename (not `@`). For example:

```
/etc/systemd/system/getty.target.wants/serial-getty%40ttyS0.service
```

This is a real filename with the characters `%`, `4`, `0` in it. It is
NOT URL encoding — it's how systemd represents `@` in unit filenames.

## Bug 1: Missing URL-Decode in List Response

### Root Cause

The encoding chain works correctly through PUT, HEAD, and GET:

1. `s3-upload` reads filename `serial-getty%40ttyS0.service` from disk
2. `rusty-s3` URL-encodes `%` → `%25` in the presigned URL path,
   producing `serial-getty%2540ttyS0.service` in the HTTP request
3. `s3s` (the S3 protocol library used by RustFS) calls
   `urlencoding::decode()` on the URI path, correctly recovering the
   key as `serial-getty%40ttyS0.service`
4. RustFS stores the key correctly

The bug is in the **LIST response path**:

5. `rusty-s3` hardcodes `encoding-type=url` in ListObjectsV2 queries
   (`rusty-s3/src/actions/list_objects_v2.rs:100`)
6. RustFS returns URL-encoded keys in the XML:
   `<Key>serial-getty%2540ttyS0.service</Key>`
   with `<EncodingType>url</EncodingType>`
7. **`s3-rootfs/src/s3.rs:parse_list_response()` extracts `<Key>` values
   without URL-decoding them** — this is the bug
8. The FUSE layer uses the raw `%2540` as the filename, which doesn't
   match the original `%40`

### What Was Ruled Out

- **rusty-s3**: correctly URL-encodes the key path for HTTP requests.
  The `FRAGMENT` charset in `signing/util.rs` includes `%`, so `%40`
  becomes `%2540` in the URL — this is correct RFC 3986 behavior.
  `Url::join()` does not double-encode on top of this.

- **ureq**: passes presigned URLs through to HTTP without re-encoding.
  The `http::Uri` type stores the raw string; `ureq-proto` writes it
  as-is to the request line.

- **RustFS / s3s server**: correctly decodes the URL path before
  storing the key. Verified by uploading via the AWS CLI (which also
  sends `%2540` on the wire) — the stored key is `%40`. The s3s
  library's `prepare()` function in `ops/mod.rs:305` calls
  `urlencoding::decode(req.uri.path())` for all request types
  (header-auth and presigned).

### Fix

In `s3-rootfs/src/s3.rs`, `parse_list_response()` must URL-decode
`<Key>` and `<Prefix>` values extracted from the XML, since the
response uses `EncodingType=url`.

Add `percent-encoding` to `s3-rootfs/Cargo.toml` and decode:

```rust
use percent_encoding::percent_decode_str;

// in parse_list_response(), after extracting the key:
let key = percent_decode_str(&key).decode_utf8_lossy().into_owned();
```

## Bug 2: No List Pagination

### Root Cause

`S3Client::list()` in `s3-rootfs/src/s3.rs:221` does a single
ListObjectsV2 call with `max-keys=1000` and returns whatever that
single page contains. It never checks `IsTruncated` or follows
`NextContinuationToken`.

A typical Debian rootfs has ~30,000–50,000 files. While `list()` is
called per-directory with `delimiter=/`, deep directories like
`/usr/share/doc/` or `/usr/lib/x86_64-linux-gnu/` can exceed 1000
entries. Those directories would appear to have files missing.

### Fix

Loop on `IsTruncated=true`, passing `NextContinuationToken` into
each subsequent request:

```rust
pub fn list(&self, prefix: &str, delimiter: Option<&str>) -> Result<ListResult, S3Error> {
    let mut all_objects = Vec::new();
    let mut all_prefixes = Vec::new();
    let mut continuation_token: Option<String> = None;

    loop {
        let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
        action.query_mut().insert("prefix", prefix);
        if let Some(d) = delimiter {
            action.query_mut().insert("delimiter", d);
        }
        action.query_mut().insert("max-keys", "1000");
        if let Some(ref token) = continuation_token {
            action.query_mut().insert("continuation-token", token);
        }

        let url = action.sign(self.sign_duration);
        let response = self.agent.get(url.as_str()).call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        let body = response.into_body().read_to_string()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        let page = parse_list_response(&body)?;
        all_objects.extend(page.objects);
        all_prefixes.extend(page.prefixes);

        // Check if there are more pages
        let is_truncated = /* extract IsTruncated from body */;
        let next_token = /* extract NextContinuationToken from body */;
        if !is_truncated || next_token.is_none() {
            break;
        }
        continuation_token = next_token;
    }

    Ok(ListResult { objects: all_objects, prefixes: all_prefixes })
}
```

### Verified Not a RustFS Bug

Pagination was tested against RustFS alpha.90 with 20,000 objects in a
rootfs-like directory tree (deep paths, special characters). All pages
returned correctly via `NextContinuationToken`, all keys round-tripped
including `%40` and space characters. RustFS pagination works; our
client just never asks for page 2.

## Reproduction

A PoC is in `../s3/poc/` (run against a local RustFS on port 19090):

```
cargo run                                         # percent-encoding round-trip
cargo run -- --list-only                          # raw vs decoded XML keys
cargo run -- --pagination --count 5000            # pagination test
cargo run -- --pagination --count 20000 --page-size 1000  # stress test
```

The percent-encoding test confirms:
- Raw XML key: `"serial-getty%2540ttyS0.service"`
- After percent_decode: `"serial-getty%40ttyS0.service"` → MATCH

The pagination test confirms:
- 20,000 objects uploaded, 100% retrieved across all pages
- All special-char keys (`%40`, spaces, `+`) survived the round-trip

## Impact

- systemd template instance services can't be enabled via `.wants` symlinks
- Any file with `%` in its name gets a wrong name in readdir
- Directories with >1000 entries are truncated silently
- Affects Debian/Ubuntu systems which use `%`-encoded systemd unit filenames

## Additional Issue: rusty-s3 + MinIO Incompatibility

When we switched from RustFS to MinIO, ALL PUT requests from our
Rust s3-upload tool return HTTP 400. The aws CLI works fine against
the same MinIO instance. This suggests rusty-s3's presigned URL
generation doesn't match what MinIO expects.

Possible causes:
- Signature v4 canonical request format differences
- URL encoding in the signing key doesn't match the request URL
- Path-style URL format not matching MinIO's expectations
- Content-Length or other header mismatch in the signed request

The aws CLI (which uses the official AWS SDK) works perfectly,
confirming MinIO's S3 implementation is correct. The issue is
in our client stack (rusty-s3 + ureq).

## Current Status

- RustFS: uploads and pagination work correctly server-side
- Both list bugs are client-side in `s3-rootfs/src/s3.rs`
- MinIO: listing works but rusty-s3 PUT requests all fail with 400
- The VMM's S3Fs runtime (reads via ureq) needs testing against MinIO
