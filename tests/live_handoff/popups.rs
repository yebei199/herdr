//! Exercise auxiliary PTYs through the real client command and server handoff path.

use std::collections::HashMap;

use super::{
    assert_ok, cleanup_test_base, client_shell_handshake, fs, register_runtime_dir, request,
    spawn_server_with_env, support, test_lock, unique_test_dir, wait_for_api,
    wait_for_file_contains, wait_for_pid_marker, wait_for_replacement_server_pid, wait_for_socket,
    Duration, Instant, Path, UnixStream,
};

fn commands(socket: &Path) -> HashMap<String, String> {
    let mut client = UnixStream::connect(socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (_, error) = client_shell_handshake(
        &mut client,
        support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        100,
        40,
    )
    .unwrap();
    assert!(error.is_none(), "client handshake: {error:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "command snapshot did not arrive");
        let (tag, payload) = support::read_server_message(&mut client).unwrap();
        if tag != support::SERVER_MESSAGE_ENDPOINT_CONTROL {
            continue;
        }
        let ((kind, data), _): ((String, String), _) =
            bincode::serde::decode_from_slice(&payload, bincode::config::standard()).unwrap();
        if kind != "shell.snapshot.v1" {
            continue;
        }
        let snapshot: serde_json::Value = serde_json::from_str(&data).unwrap();
        return snapshot["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| {
                (
                    command["binding_label"].as_str().unwrap().to_owned(),
                    command["command_id"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
    }
}

fn invoke(socket: &Path, command: &str, tab: &str) {
    assert_ok(request(
        socket,
        serde_json::json!({
            "id": "invoke", "method": "command.invoke",
            "params": {"command_id": command, "tab_id": tab}
        }),
    ));
}

fn process_start(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap()
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_owned()
}

fn tick(directory: &Path) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = fs::read_to_string(directory.join("tick")).unwrap_or_default();
        if let Ok(number) = value.trim().parse() {
            return number;
        }
        assert!(
            Instant::now() < deadline,
            "loop did not publish a complete tick"
        );
        std::thread::yield_now();
    }
}

#[cfg(target_os = "linux")]
fn check_auxiliary_handoff(failure: bool, visible_popup: bool) {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let config_path = config_home.join("herdr/config.toml");
    let mut extra_env = vec![("HERDR_CONFIG_PATH", config_path.to_str().unwrap())];
    if failure {
        extra_env.push(("HERDR_TEST_HANDOFF_IMPORT_FAIL", "after_restored"));
    }
    let spawned = spawn_server_with_env(&config_home, &runtime_dir, &api_socket, &extra_env);
    let mut server_pid = spawned.child.process_id().unwrap();
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let script = base.join("aux.sh");
    fs::write(
        &script,
        r#"state=retained
sleep 600 &
worker=$!
trap 'kill "$worker"' EXIT
printf 'READY %s\n' "$$" > shell.pid
printf 'READY %s\n' "$worker" > worker.pid
i=0
while :; do
  if IFS= read -r -t 0.05 line; then printf '%s:%s\n' "$state" "$line" > received; fi
  i=$((i + 1))
  printf '%s\n' "$i" > tick
done
"#,
    )
    .unwrap();
    let command = format!("bash {}", script.display());
    fs::write(config_home.join("herdr/config.toml"), format!(
        "onboarding = false\n[[keys.command]]\nkey = 'alt+t'\ntype = 'float'\ncommand = {0:?}\n[[keys.command]]\nkey = 'alt+p'\ntype = 'popup'\ncommand = {0:?}\n", command
    )).unwrap();
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"reload", "method":"server.reload_config", "params":{}}),
    ));
    let manifest = commands(&client_socket);
    let mut terminals = Vec::new();
    for index in 0..3 {
        let directory = base.join(format!("terminal-{index}"));
        fs::create_dir_all(&directory).unwrap();
        let created = request(
            &api_socket,
            serde_json::json!({
                "id":"create", "method":"workspace.create", "params":{"cwd":directory,"focus":true}
            }),
        );
        assert_ok(created.clone());
        let tab = created["result"]["root_pane"]["tab_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let binding = if index == 2 && visible_popup {
            "alt+p"
        } else {
            "alt+t"
        };
        invoke(&api_socket, &manifest[binding], &tab);
        let shell = wait_for_pid_marker(&directory.join("shell.pid"), Duration::from_secs(5));
        let worker = wait_for_pid_marker(&directory.join("worker.pid"), Duration::from_secs(5));
        if index != 2 {
            invoke(&api_socket, &manifest["alt+t"], &tab);
        }
        terminals.push((
            directory,
            tab,
            shell,
            process_start(shell),
            worker,
            process_start(worker),
        ));
    }
    for _ in 0..2 {
        let ticks: Vec<_> = terminals.iter().map(|entry| tick(&entry.0)).collect();
        let result = request(
            &api_socket,
            serde_json::json!({"id":"handoff", "method":"server.live_handoff", "params":{}}),
        );
        if failure {
            assert!(result.get("error").is_some(), "{result}");
        } else {
            assert_ok(result);
            server_pid =
                wait_for_replacement_server_pid(&runtime_dir, server_pid, Duration::from_secs(10));
        }
        wait_for_api(&api_socket, Duration::from_secs(10));
        for ((directory, _, shell, shell_start, worker, worker_start), before) in
            terminals.iter().zip(ticks)
        {
            assert_eq!(
                process_start(*shell),
                *shell_start,
                "auxiliary shell restarted"
            );
            assert_eq!(
                process_start(*worker),
                *worker_start,
                "auxiliary child restarted"
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while tick(directory) <= before {
                assert!(Instant::now() < deadline, "auxiliary loop stopped");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    if !visible_popup {
        let manifest = commands(&client_socket);
        invoke(&api_socket, &manifest["alt+t"], &terminals[2].1);
        for (directory, tab, shell, start, _, _) in &terminals {
            invoke(&api_socket, &manifest["alt+t"], tab);
            invoke(&api_socket, &manifest["alt+t"], tab);
            assert_eq!(process_start(*shell), *start);
            let marker = wait_for_file_contains(
                &directory.join("shell.pid"),
                "READY",
                Duration::from_secs(5),
            );
            assert!(marker.contains(&shell.to_string()));
        }
    }
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"stop", "method":"server.stop", "params":{}}),
    ));
    drop(spawned);
    cleanup_test_base(&base);
}

#[cfg(target_os = "linux")]
#[test]
fn live_handoff_preserves_visible_and_hidden_floats() {
    check_auxiliary_handoff(false, false);
}

#[cfg(target_os = "linux")]
#[test]
fn live_handoff_preserves_visible_popup_and_hidden_floats() {
    check_auxiliary_handoff(false, true);
}

#[cfg(target_os = "linux")]
#[test]
fn live_handoff_float_rollback_preserves_processes() {
    check_auxiliary_handoff(true, false);
}
