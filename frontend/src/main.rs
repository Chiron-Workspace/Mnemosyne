use leptos::prelude::*;
use serde::{Deserialize, Serialize};

const API_BASE: &str = "http://localhost:8081";

// ---------------------------------------------------------------------------
// API types — matching backend structs directly from source:
//   backend/src/handlers/socratic.rs (StartRequest, StartResponse, ReplyRequest, ReplyResponse)
//   backend/src/handlers/users.rs    (UserRow → User)
//   backend/src/handlers/study_sets.rs (StudySetRow → StudySet)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
struct User {
    id: String,
    email: String,
}

#[derive(Debug, Deserialize, Clone)]
struct StudySet {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
struct StartRequest {
    study_set_id: String,
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct StartResponse {
    session_id: String,
    opening_message: String,
}

#[derive(Debug, Serialize)]
struct ReplyRequest {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ReplyResponse {
    reply: String,
    flagged_misconception: Option<String>,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: String,
    content: String,
    flagged_misconception: Option<String>,
}

// ---------------------------------------------------------------------------
// API helpers
// ---------------------------------------------------------------------------

async fn api_get<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let resp = gloo_net::http::Request::get(&format!("{API_BASE}{path}"))
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<T>()
        .await
        .map_err(|e| format!("parse error: {e}"))
}

async fn api_post<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    path: &str,
    body: &T,
) -> Result<R, String> {
    let body_str = serde_json::to_string(body).map_err(|e| format!("serialize: {e}"))?;
    let resp = gloo_net::http::Request::post(&format!("{API_BASE}{path}"))
        .header("Content-Type", "application/json")
        .body(body_str)
        .map_err(|e| format!("body error: {e}"))?
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.ok() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", resp.status(), text));
    }
    resp.json::<R>()
        .await
        .map_err(|e| format!("parse error: {e}"))
}

// ---------------------------------------------------------------------------
// App — switches between setup and chat phases via a signal
// ---------------------------------------------------------------------------

#[component]
fn App() -> impl IntoView {
    let (phase, set_phase) = signal("setup".to_string());
    let (session_id, set_session_id) = signal(String::new());
    let (initial_messages, set_initial_messages) = signal(Vec::<ChatMessage>::new());

    view! {
        <main class="container">
            <div class="card">
                <h1>Mnemosyne</h1>
                {move || {
                    if phase.get() == "setup" {
                        view! {
                            <SetupPhase on_start=move |sid, msgs| {
                                set_session_id.set(sid);
                                set_initial_messages.set(msgs);
                                set_phase.set("chat".into());
                            } />
                        }
                        .into_any()
                    } else {
                        view! {
                            <ChatPhase
                                session_id=session_id.get()
                                initial_messages=initial_messages.get()
                                on_end=move |_| set_phase.set("setup".into())
                            />
                        }
                        .into_any()
                    }
                }}
            </div>
        </main>
    }
}

// ---------------------------------------------------------------------------
// Setup phase — pick user + study set, start session
// ---------------------------------------------------------------------------

#[component]
fn SetupPhase(on_start: impl Fn(String, Vec<ChatMessage>) + 'static) -> impl IntoView {
    // Wrap in a Copy UnsyncCallback so the closure body can call it without
    // moving it. UnsyncCallback is correct for CSR (no Send+Sync needed).
    let on_start: UnsyncCallback<(String, Vec<ChatMessage>), ()> = UnsyncCallback::from(on_start);
    let (users, set_users) = signal(Vec::<User>::new());
    let (selected_user, set_selected_user) = signal(String::new());
    let (study_sets, set_study_sets) = signal(Vec::<StudySet>::new());
    let (selected_set, set_selected_set) = signal(String::new());
    let (error, set_error) = signal(String::new());
    let (starting, set_starting) = signal(false);

    leptos::task::spawn_local(async move {
        match api_get::<Vec<User>>("/users").await {
            Ok(u) => set_users.set(u),
            Err(e) => set_error.set(format!("Failed to load users: {e}")),
        }
    });

    let on_user_change = move |e| {
        let uid = event_target_value(&e);
        set_selected_user.set(uid.clone());
        set_study_sets.set(Vec::new());
        set_selected_set.set(String::new());
        if uid.is_empty() {
            return;
        }
        leptos::task::spawn_local(async move {
            let path = format!("/study_sets?user_id={uid}");
            match api_get::<Vec<StudySet>>(&path).await {
                Ok(s) => set_study_sets.set(s),
                Err(e) => set_error.set(format!("Failed to load study sets: {e}")),
            }
        });
    };

    let do_start = move |_| {
        let uid = selected_user.get();
        let sid = selected_set.get();
        if uid.is_empty() || sid.is_empty() {
            return;
        }
        set_starting.set(true);
        set_error.set(String::new());
        leptos::task::spawn_local(async move {
            let req = StartRequest { study_set_id: sid.clone(), user_id: uid.clone() };
            match api_post::<StartRequest, StartResponse>("/socratic/start", &req).await {
                Ok(resp) => {
                    let msgs = vec![ChatMessage {
                        role: "assistant".into(),
                        content: resp.opening_message,
                        flagged_misconception: None,
                    }];
                    on_start.run((resp.session_id, msgs));
                }
                Err(e) => {
                    set_error.set(e);
                    set_starting.set(false);
                }
            }
        });
    };

    let can_start = move || {
        !selected_user.get().is_empty() && !selected_set.get().is_empty() && !starting.get()
    };

    view! {
        <div class="setup">
            <label>"User"</label>
            <select on:change=on_user_change prop:value=move || selected_user.get()>
                <option value="">"-- Select a user --"</option>
                {move || users.get().iter().map(|u| {
                    view! { <option value={u.id.clone()}>{u.email.clone()}</option> }
                }).collect::<Vec<_>>()}
            </select>

            <label>"Study Set"</label>
            <select on:change=move |e| set_selected_set.set(event_target_value(&e)) prop:value=move || selected_set.get()>
                <option value="">"-- Select a study set --"</option>
                {move || study_sets.get().iter().map(|s| {
                    view! { <option value={s.id.clone()}>{s.name.clone()}</option> }
                }).collect::<Vec<_>>()}
            </select>

            <button on:click=do_start disabled=move || !can_start()>
                {move || if starting.get() { "Starting..." } else { "Start Socratic Session" }}
            </button>

            {move || {
                let e = error.get();
                if e.is_empty() {
                    Vec::<AnyView>::new()
                } else {
                    vec![view! { <p class="error-text">{e}</p> }.into_any()]
                }
            }}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Chat phase — conversation + reply input
// ---------------------------------------------------------------------------

#[component]
fn ChatPhase(
    session_id: String,
    initial_messages: Vec<ChatMessage>,
    on_end: impl Fn(()) + 'static,
) -> impl IntoView {
    let on_end = UnsyncCallback::new(on_end);
    let (messages, set_messages) = signal(initial_messages);
    let (input_text, set_input_text) = signal(String::new());
    let (sending, set_sending) = signal(false);
    let (error, set_error) = signal(String::new());

    // Wrap the send logic in an UnsyncCallback<()> so it can be shared between
    // the click handler (MouseEvent) and the keydown handler (KeyboardEvent)
    // without moving the same closure twice. UnsyncCallback<()> is Copy.
    // Unsync (no Send+Sync needed) is correct for CSR/WASM which is
    // single-threaded.
    let do_send = UnsyncCallback::new(move |_: ()| {
        let text = input_text.get().trim().to_string();
        if text.is_empty() || sending.get() {
            return;
        }
        set_sending.set(true);
        set_error.set(String::new());
        set_input_text.set(String::new());

        // Optimistic: append the user's message immediately.
        set_messages.update(|m| {
            m.push(ChatMessage {
                role: "user".into(),
                content: text.clone(),
                flagged_misconception: None,
            })
        });

        let sid = session_id.clone();
        leptos::task::spawn_local(async move {
            let req = ReplyRequest { message: text };
            let path = format!("/socratic/{sid}/reply");
            match api_post::<ReplyRequest, ReplyResponse>(&path, &req).await {
                Ok(resp) => {
                    set_messages.update(|m| {
                        m.push(ChatMessage {
                            role: "assistant".into(),
                            content: resp.reply,
                            flagged_misconception: resp.flagged_misconception,
                        })
                    });
                }
                Err(e) => {
                    set_error.set(e);
                }
            }
            set_sending.set(false);
        });
    });

    view! {
        <div class="chat">
            <div class="chat-messages">
                {move || {
                    messages.get().iter().map(|m| {
                        let role = m.role.clone();
                        let content = m.content.clone();
                        let flag = m.flagged_misconception.clone();
                        view! {
                            <div class={move || format!("msg msg-{role}")}>
                                <p>{content}</p>
                                {move || {
                                    let f = flag.clone();
                                    if let Some(fm) = f {
                                        vec![view! {
                                            <span class="misconception-badge">
                                                "⚠ misconception noted: " {fm}
                                            </span>
                                        }.into_any()]
                                    } else {
                                        Vec::<AnyView>::new()
                                    }
                                }}
                            </div>
                        }
                    }).collect::<Vec<_>>()
                }}
            </div>

            {move || {
                let e = error.get();
                if e.is_empty() {
                    Vec::<AnyView>::new()
                } else {
                    vec![view! { <p class="error-text">{e}</p> }.into_any()]
                }
            }}

            <div class="chat-input">
                <input
                    type="text"
                    prop:value=move || input_text.get()
                    on:input=move |e| set_input_text.set(event_target_value(&e))
                    on:keydown=move |e| {
                        if e.key() == "Enter" && !sending.get() {
                            do_send.run(());
                        }
                    }
                    disabled=move || sending.get()
                    placeholder="Type your reply..."
                />
                <button on:click=move |_| do_send.run(()) disabled=move || sending.get()>
                    {move || if sending.get() { "Sending..." } else { "Send" }}
                </button>
            </div>

            <button class="end-btn" on:click=move |_| on_end.run(())>
                "End session (start over)"
            </button>
        </div>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}