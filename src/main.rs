// Copyright 2021-2022 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

#[macro_use]
extern crate zbus;

mod config;
mod cpu;
mod dbus;
mod nice;
mod paths;

use std::{collections::BTreeMap, path::Path, time::Duration};

use crate::config::{cpu::Config as CpuConfig, Config};
use crate::paths::SchedPaths;
use argh::FromArgs;
use dbus::{CpuMode, Server};
use tokio::sync::mpsc::Sender;
use upower_dbus::UPowerProxy;
use zbus::{Connection, PropertyStream};

#[derive(FromArgs, PartialEq, Debug)]
/// System76 Scheduler Tweaker
struct Args {
    #[argh(subcommand)]
    subcmd: SubCmd,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCmd {
    Cpu(CpuArgs),
    Daemon(DaemonArgs),
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "cpu")]
/// Change the CPU scheduler configuration.
struct CpuArgs {
    #[argh(positional)]
    profile: Option<String>,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "daemon")]
/// Launch the DBus service.
struct DaemonArgs {}

#[derive(Debug)]
enum Event {
    OnBattery(bool),
    SetAutoBackgroundPriority(u32, String),
    SetCpuMode,
    SetCustomCpuMode,
    SetForegroundProcess(u32),
    UpdateProcessMap(BTreeMap<u32, Option<u32>>),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let connection = Connection::system().await?;

    let args: Args = argh::from_env();

    match args.subcmd {
        SubCmd::Cpu(args) => cpu(connection, args).await,
        SubCmd::Daemon(_) => daemon(connection).await,
    }
}

async fn cpu(connection: Connection, args: CpuArgs) -> anyhow::Result<()> {
    let mut connection = dbus::ClientProxy::new(&connection).await?;

    match args.profile.as_ref() {
        Some(profile) => connection.set_cpu_profile(profile).await?,
        None => println!("{}", connection.cpu_profile().await?),
    }

    Ok(())
}

async fn daemon(connection: Connection) -> anyhow::Result<()> {
    let paths = SchedPaths::new()?;

    let upower_proxy = UPowerProxy::new(&connection).await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);

    connection
        .object_server()
        .at(
            "/com/system76/Scheduler",
            Server {
                cpu_mode: CpuMode::Auto,
                cpu_profile: String::from("auto"),
                tx: tx.clone(),
            },
        )
        .await?;

    connection.request_name("com.system76.Scheduler").await?;

    tokio::spawn(battery_monitor(
        upower_proxy.receive_on_battery_changed().await,
        tx.clone(),
    ));

    let apply_config = |on_battery: bool| {
        cpu::tweak(
            &paths,
            &if on_battery {
                eprintln!("auto config applying default config");
                CpuConfig::default_config()
            } else {
                eprintln!("auto config applying responsive config");
                CpuConfig::responsive_config()
            },
        );
    };

    apply_config(upower_proxy.on_battery().await.unwrap_or(false));

    let mut process_monitoring_handle = None;
    let mut foreground_processes: Option<Vec<u32>> = None;
    let mut process_map = BTreeMap::new();

    let config = Config::read();
    let automatic_assignments = Config::automatic_assignments();

    while let Some(event) = rx.recv().await {
        let interface_result = connection
            .object_server()
            .interface::<_, Server>("/com/system76/Scheduler")
            .await;

        let iface_handle = match interface_result {
            Ok(iface_handler) => iface_handler,
            Err(why) => {
                eprintln!("DBus interface not reachable: {:#?}", why);
                break;
            }
        };

        let interface = iface_handle.get().await;

        match event {
            Event::SetAutoBackgroundPriority(pid, exe) => {
                if let Some(current) = foreground_processes.as_ref() {
                    if current.contains(&pid) {
                        continue;
                    }
                }

                let current = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };

                let priority = automatic_assignments
                    .get(&exe)
                    .cloned()
                    .or(config.background);

                if let Some(mut priority) = priority {
                    if priority < -9 {
                        priority = -9;
                    }

                    if current >= -9 && current <= 9 {
                        crate::nice::set_priority(pid, priority as i32)
                    }
                }
            }

            Event::UpdateProcessMap(map) => {
                process_map = map;
            }

            Event::SetForegroundProcess(pid) => {
                eprintln!("SetForegroundProcess({pid})");
                if let Some(foreground_priority) = config.foreground {
                    if let Some(prev) = foreground_processes.take() {
                        let priority = config.background.unwrap_or(0) as i32;
                        for process in prev {
                            crate::nice::set_priority(process, priority);
                        }
                    } else if process_monitoring_handle.is_none() {
                        process_monitoring_handle =
                            Some(tokio::spawn(process_monitor(tx.clone(), pid)));
                        continue;
                    }

                    crate::nice::set_priority(pid, foreground_priority as i32);
                    let mut processes = vec![pid];

                    'outer: loop {
                        for (pid, parent) in process_map.iter() {
                            if let Some(parent) = parent {
                                if processes.contains(parent) && !processes.contains(pid) {
                                    if let Some(exe) = exe_of_pid(*pid) {
                                        let priority = automatic_assignments
                                            .get(&exe)
                                            .cloned()
                                            .unwrap_or(foreground_priority)
                                            as i32;

                                        crate::nice::set_priority(*pid, priority);
                                    }

                                    processes.push(*pid);
                                    continue 'outer;
                                }
                            }
                        }

                        break;
                    }

                    foreground_processes = Some(processes);
                }
            }

            Event::OnBattery(on_battery) => {
                if let CpuMode::Auto = interface.cpu_mode {
                    apply_config(on_battery);
                }
            }

            Event::SetCpuMode => match interface.cpu_mode {
                CpuMode::Auto => {
                    eprintln!("applying auto config");
                    apply_config(upower_proxy.on_battery().await.unwrap_or(false));
                }

                CpuMode::Default => {
                    eprintln!("applying default config");
                    cpu::tweak(&paths, &CpuConfig::default_config());
                }

                CpuMode::Responsive => {
                    eprintln!("applying responsive config");
                    cpu::tweak(&paths, &CpuConfig::responsive_config());
                }

                _ => (),
            },

            Event::SetCustomCpuMode => {
                if let Some(config) = CpuConfig::custom_config(&interface.cpu_profile) {
                    eprintln!("applying {} config", interface.cpu_profile);
                    cpu::tweak(&paths, &config);
                }
            }
        }
    }
    Ok(())
}

async fn battery_monitor(mut events: PropertyStream<'_, bool>, tx: Sender<Event>) {
    use futures::StreamExt;
    while let Some(event) = events.next().await {
        if let Ok(on_battery) = event.get().await {
            let _ = tx.send(Event::OnBattery(on_battery)).await;
        }
    }
}

async fn process_monitor(tx: Sender<Event>, foreground: u32) {
    let mut initial = Some(foreground);

    loop {
        if let Ok(procfs) = Path::new("/proc").read_dir() {
            let mut parents = BTreeMap::<u32, Option<u32>>::new();

            for proc_entry in procfs.filter_map(Result::ok) {
                let proc_path = proc_entry.path();

                let pid = if let Some(pid) = proc_path
                    .file_name()
                    .and_then(|p| p.to_str())
                    .and_then(|p| p.parse::<u32>().ok())
                {
                    pid
                } else {
                    continue;
                };

                let mut parent = None;

                if let Ok(status) = tokio::fs::read_to_string(proc_path.join("status")).await {
                    for line in status.lines() {
                        if let Some(ppid) = line.strip_prefix("PPid:") {
                            if let Ok(ppid) = ppid.trim().parse::<u32>() {
                                parent = Some(ppid);
                            }

                            break;
                        }
                    }
                }

                parents.insert(pid, parent);

                // Prevents kernel processes from having their priorities changed.
                if let Ok(exe) = proc_path.join("exe").canonicalize() {
                    if let Some(exe) = exe.file_name().and_then(|x| x.to_str()).map(String::from) {
                        let _ = tx.send(Event::SetAutoBackgroundPriority(pid, exe)).await;
                    }

                    tokio::task::yield_now().await;
                }
            }

            let _ = tx.send(Event::UpdateProcessMap(parents)).await;
            tokio::task::yield_now().await;
        }

        if let Some(pid) = initial.take() {
            let _ = tx.send(Event::SetForegroundProcess(pid)).await;
            tokio::task::yield_now().await;
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

fn exe_of_pid(pid: u32) -> Option<String> {
    if let Ok(exe) = Path::new(&format!("/proc/{}/exe", pid)).canonicalize() {
        if let Some(exe) = exe.file_name().and_then(|x| x.to_str()).map(String::from) {
            return Some(exe);
        }
    }

    None
}
