//! Disposable release-suite bucket setup, using credentials only from the environment.
use anyhow::{Context,Result};
use layr_store::s3::S3;
fn main()->Result<()> {
 let bucket=std::env::args().nth(1).context("test bucket")?;
 let env=std::env::vars().collect();let s3=S3::with_env(&bucket,"",&env)?;
 let response=s3.request(reqwest::Method::PUT,"",&[],&[],vec![],std::time::Duration::from_secs(30))?;
 anyhow::ensure!(response.status().is_success(),"test bucket creation: HTTP {}",response.status());
 Ok(())
}
