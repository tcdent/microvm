# s3-rootfs TODO

## In Progress
- [ ] Symlink support (readlink, symlink detection in lookup)

## Not Yet Implemented
- [ ] Symlink creation (FUSE SYMLINK op)
- [ ] Rename (FUSE RENAME op → CopyObject + DeleteObject)
- [ ] Hardlinks (return ENOTSUP, document why)
- [ ] chmod/chown persistence (setattr writes metadata back to S3)
- [ ] Truncate (resize file in write buffer or PUT empty)
- [ ] statfs (return reasonable fixed values for df)
- [ ] fsync/fdatasync (flush dirty buffer to S3 immediately)
- [ ] rmdir (DELETE directory marker)
- [ ] Negative lookup caching (avoid repeated S3 HEAD for missing files)
- [ ] POSIX metadata on write (preserve mode/uid/gid from create/mkdir through to S3)
- [ ] Debounce/periodic flush for long-lived open files

## Performance
- [ ] Parallel S3 calls for readdir (batch HEAD requests)
- [ ] Connection pool tuning (expose knobs for max connections, keepalive)
- [ ] Inode table preloading (ListObjects on mount instead of per-lookup)

## Future
- [ ] inotify plumbing (FUSE_NOTIFY_INVAL_ENTRY for external file changes)
- [ ] Large file chunking (>100MB, via metadata pointers)
- [ ] Template bucket fallback (CoW reads: check vm bucket, fall through to template)
