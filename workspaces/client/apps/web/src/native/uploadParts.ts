/**
 * Largest decoded part of one `files.append`, in bytes.
 * Owner: client-core `uploads.rs` `PART_LIMIT`. The parity test in uploads.test.ts checks it.
 */
export const UPLOAD_PART_BYTES = 1024 * 1024;
