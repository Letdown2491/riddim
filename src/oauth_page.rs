use base64::Engine as _;

pub(crate) fn success_page() -> String {
    let brand_mark = base64::engine::general_purpose::STANDARD
        .encode(include_bytes!("../assets/riddim-mark.png"));

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>Riddim is ready</title>
  <style>
    :root {{ color-scheme: light dark; font-family: Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    * {{ box-sizing: border-box; }}
    body {{ min-height: 100vh; margin: 0; display: grid; place-items: center; background: #fbfaf9; color: #171717; }}
    main {{ width: min(480px, calc(100vw - 40px)); padding: 32px; border: 1px solid #e8e8e8; border-radius: 16px; background: #fff; box-shadow: 0 12px 32px rgba(0,0,0,.06); }}
    .brand {{ margin-bottom: 40px; display: flex; align-items: center; gap: 10px; font-size: 17px; font-weight: 700; }}
    .brand img {{ width: 32px; height: 32px; border-radius: 10px; }}
    h1 {{ margin: 0; font-size: 28px; line-height: 1.15; letter-spacing: -.02em; }}
    p {{ margin: 12px 0 0; color: #494440; font-size: 15px; line-height: 1.55; }}
    @media (prefers-color-scheme: dark) {{
      body {{ background: #121212; color: #f5f3ef; }}
      main {{ border-color: #414141; background: #1a1a1a; box-shadow: 0 20px 60px rgba(0,0,0,.35); }}
      .step, p {{ color: #d5d1cb; }}
    }}
  </style>
</head>
<body>
  <main>
    <div class="brand"><img src="data:image/png;base64,{brand_mark}" alt="">Riddim</div>
    <h1>Riddim is ready</h1>
    <p>Authorization is complete. Close this page and return to Riddim.</p>
  </main>
</body>
</html>"##
    )
}

#[cfg(test)]
mod tests {
    use super::success_page;

    #[test]
    fn callback_page_reports_completion() {
        let page = success_page();
        assert!(page.contains("Riddim is ready"));
        assert!(page.contains("Close this page"));
    }
}
