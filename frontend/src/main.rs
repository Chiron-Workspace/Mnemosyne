use leptos::prelude::*;

#[component]
fn App() -> impl IntoView {
    let (status_text, set_status_text) = signal("Checking backend...".to_string());
    let (status_class, set_status_class) = signal("loading".to_string());

    leptos::task::spawn_local(async move {
        let result = async {
            let resp = gloo_net::http::Request::get("http://localhost:8081/health")
                .send()
                .await
                .map_err(|e| format!("network error: {e}"))?;
            if !resp.ok() {
                return Err(format!("backend responded HTTP {}", resp.status()));
            }
            let body = resp.text().await.map_err(|e| format!("{e}"))?;
            Ok(body)
        }
        .await;

        match result {
            Ok(body) if body.trim() == "ok" => {
                set_status_text.set("Backend: ok".into());
                set_status_class.set("ok".into());
            }
            Ok(other) => {
                set_status_text.set(format!("unexpected response: {other}").into());
                set_status_class.set("error".into());
            }
            Err(err) => {
                set_status_text.set(format!("Backend unreachable: {err}").into());
                set_status_class.set("error".into());
            }
        }
    });

    view! {
        <main class="container">
            <div class="card">
                <h1>Mnemosyne</h1>
                <p class=move || format!("status status-{}", status_class.get())>
                    {move || status_text.get()}
                </p>
            </div>
        </main>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}