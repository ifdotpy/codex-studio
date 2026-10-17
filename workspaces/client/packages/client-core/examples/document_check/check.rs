//! Hidden system renderer check. No user state, visible window or OS input.
use base64::{Engine, engine::general_purpose::STANDARD};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use studio_client_core::{protocol::Request, rich_document};
async fn render(source: &str, page: usize) -> Result<Value, String> {
    let response = rich_document::dispatch(Request {
        id: format!("page-{page}"),
        method: "document.render".into(),
        params: json!({"source":source,"page":page}),
    })
    .await;
    response
        .result
        .ok_or_else(|| response.error.unwrap().message)
}
fn pixels(value: &Value) -> Vec<u8> {
    STANDARD
        .decode(
            value["url"]
                .as_str()
                .unwrap()
                .strip_prefix("data:image/png;base64,")
                .unwrap(),
        )
        .unwrap()
}
async fn check() -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let count = calls.clone();
    let done = stopped.clone();
    let server = std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                count.fetch_add(1, Ordering::Relaxed);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
    let source = format!(
        r#"<html><head><meta http-equiv="Content-Security-Policy" content="default-src * 'unsafe-inline'"><style>@import url('http://{address}/css');html,body{{margin:0}}body{{background:red}}div{{height:600px}}#second{{background:blue}}</style></head><body onload="document.body.style.background='lime'"><div>Rust HTML Ж🦀</div><div id="second">Second page</div><script>document.body.style.background='lime';fetch('http://{address}/script')</script><img src="http://{address}/image"><iframe src="http://{address}/frame"></iframe></body></html>"#
    );
    let result=async {
        let first=render(&source,0).await?;
        let bytes=pixels(&first);
        let image=image::load_from_memory(&bytes).map_err(|e|e.to_string())?.to_rgb8();
        let rgb=image.get_pixel(image.width()/2,image.height()/2).0;
        if rgb!=[255,0,0] {return Err(format!("First page pixels: {rgb:?}"));}
        let second=render(&source,1).await?;
        let image=image::load_from_memory(&pixels(&second)).map_err(|e|e.to_string())?.to_rgb8();
        let rgb=image.get_pixel(image.width()/2,image.height()/2).0;
        if rgb!=[0,0,255] {return Err(format!("Second page pixels: {rgb:?}"));}
        let short=render("<style>html,body{margin:0}div{height:600px;background:red}#last{height:50px;background:lime}</style><div></div><div></div><div id='last'></div>",2).await?;
        let image=image::load_from_memory(&pixels(&short)).map_err(|e|e.to_string())?.to_rgb8();
        // WebKit returns device pixels; the view and response use CSS pixels.
        // Check the entire crop and its ratio at either display scale.
        if short["pages"]!=3 || short["height"]!=50 || image.height()==0
            || image.height()*960!=image.width()*50
            || image.pixels().any(|pixel|pixel.0!=[0,255,0]) {
            return Err(format!("Final HTML page: pages={} height={} pixels={}x{} top={:?} bottom={:?}",short["pages"],short["height"],image.width(),image.height(),image.get_pixel(image.width()/2,0).0,image.get_pixel(image.width()/2,image.height()-1).0));
        }
        if render(&source,79).await.is_ok() {return Err("Missing page accepted.".into());}
        if render("<div style='height:999999px'></div>",0).await.is_ok() {return Err("Unbounded page accepted.".into());}
        for source in ["flowchart LR\n A[Ж🦀] --> B[Done]", "sequenceDiagram\n Alice->>Bob: Hello", "pie title Fruit\n \"Apple\": 3\n \"Pear\": 2"] {
            let response=rich_document::dispatch(Request{id:"diagram".into(),method:"document.diagram".into(),params:json!({"source":source})}).await;
            let diagram=response.result.ok_or_else(||format!("Diagram: {:?}",response.error))?;
            let svg=diagram["svg"].as_str().ok_or("Missing diagram SVG")?;
            if !svg.starts_with("<svg") || diagram["height"].as_f64().is_none() {return Err("Invalid diagram presentation.".into());}
            let rendered=render(svg,0).await?;
            let rendered_bytes=pixels(&rendered);
            let image=image::load_from_memory(&rendered_bytes).map_err(|e|e.to_string())?.to_rgb8();
            if image.pixels().filter(|p|p.0.iter().any(|c|*c<240)).count()<2000 {return Err("Diagram has no rendered content.".into());}
            if let Some(path)=std::env::args_os().nth(1) {
                let path=std::path::PathBuf::from(path);
                let name=source.split_whitespace().next().unwrap();
                std::fs::write(path.with_file_name(format!("diagram-{name}.png")),rendered_bytes).map_err(|e|e.to_string())?;
            }
        }
        let invalid=rich_document::dispatch(Request{id:"bad-diagram".into(),method:"document.diagram".into(),params:json!({"source":"not a valid diagram"})}).await;
        if invalid.error.is_none() {return Err("Invalid diagram accepted.".into());}
        let refresh=format!("<meta http-equiv='refresh' content='0;url=http://{address}/refresh'><style>body{{background:red}}</style>");
        let refreshed=render(&refresh,0).await?;
        if pixels(&refreshed).is_empty() {return Err("Refresh removed the source document.".into());}
        tokio::time::sleep(Duration::from_millis(200)).await;
        if calls.load(Ordering::Relaxed)!=0 {return Err("The HTML document accessed the network.".into());}
        if let Some(path)=std::env::args_os().nth(1) {std::fs::write(path,bytes).map_err(|e|e.to_string())?;}
        println!("Document renderer PASS: real PNG pixels, Unicode, two pages, JavaScript disabled, no HTTP resources or refresh, bounded height and page, flowchart/sequence/pie SVG and visible invalid syntax.");
        Ok(())
    }.await;
    stopped.store(true, Ordering::Relaxed);
    server.join().unwrap();
    result
}
pub(crate) fn main() {
    let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.spawn(async {
        let result = tokio::time::timeout(Duration::from_secs(50), check()).await;
        let status = match result {
            Ok(Ok(())) => 0,
            other => {
                eprintln!("Document renderer FAILED: {other:?}");
                1
            }
        };
        std::process::exit(status);
    });
    app.run();
}
