//! Heavy third-party dependencies of the test binaries, built once as a dynamic library.
//! One `extern crate` line per dependency makes the dylib contain it.

extern crate axum;
extern crate base64;
extern crate brotli;
extern crate bytes;
extern crate chrono;
extern crate data_encoding;
extern crate flate2;
extern crate futures_util;
extern crate http_body_util;
extern crate hyper;
extern crate hyper_util;
extern crate jsonschema;
extern crate mime_guess;
extern crate notify;
extern crate process_wrap;
extern crate regex;
extern crate rustix;
extern crate serde;
extern crate serde_json;
extern crate serde_path_to_error;
extern crate sha2;
extern crate sysinfo;
extern crate tar;
extern crate tempfile;
extern crate thiserror;
extern crate tokio;
extern crate tokio_util;
extern crate toml;
extern crate tower;
extern crate tower_http;
extern crate url;
extern crate uuid;
