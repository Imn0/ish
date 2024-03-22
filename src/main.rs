use std::{
    io::{stdin, stdout, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use libc::{
    c_int, pid_t, SIGCONT, SIGINT, SIGTSTP, STDIN_FILENO, TCSADRAIN, WNOHANG,
    WUNTRACED,
};

// Empty signal handler so we don't exit on signals
extern "C" fn handle_signal(_: c_int) {}

// simple check to see if a process is running
fn is_process_running(pid: pid_t) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    result == 0
}

// Monitor background tasks and remove them from the vector when they exit
fn monitor_background_tasks(backgound_tasks: Arc<Mutex<Vec<Child>>>) {
    loop {
        // wait a bit between checks
        thread::sleep(Duration::from_millis(100));

        // Lock the mutex before accessing the vector
        let mut background_tasks = backgound_tasks.lock().unwrap();

        background_tasks.retain(|task| {
            let pid = task.id() as i32;
            let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), WNOHANG) };

            match result {
                -1 => {
                    eprintln!("Error checking status for background task {}", task.id());
                    true // Keep the task in the vector
                }
                0 => true, // Task is still running
                _ => {
                    // Task is in a Zombie state, remove it from the vector
                    println!("Background task {} exited", task.id());
                    false
                }
            }
        });
    }
}

fn main() {
    // Ignore signals so they don't kill the shell
    unsafe {
        libc::signal(SIGINT, handle_signal as usize);
        libc::signal(SIGTSTP, handle_signal as usize);
    }
    // list of current stopped processes
    let mut current_stopped: Option<Child> = None;

    // vector of background tasks
    let backgound_tasks = Arc::new(Mutex::new(Vec::new()));

    // Spawn a background thread to monitor background tasks
    let _background_thread = {
        let backgound_tasks = Arc::clone(&backgound_tasks);
        thread::spawn(move || {
            monitor_background_tasks(backgound_tasks);
        })
    };

    // main loop
    loop {
        print!("> ");
        let _ = stdout().flush(); // flush stdout so the prompt doesn't read '>'
        let mut raw_input: String = String::new(); // read input from stdin

        // exit when ^D is pressed
        match stdin().read_line(&mut raw_input) {
            Ok(0) => break, // Exit the loop on EOF (^D)
            Ok(_) => {}
            Err(err) => {
                eprintln!("Error reading input: {}", err);
                break;
            }
        }

        // check if the user wants to run the command in the background
        let mut wait = true;
        if raw_input.trim().ends_with('&') {
            wait = false;
        }

        // remove the trailing & if it exists
        let input = raw_input.trim_end().trim_end_matches('&');

        // split the input into commands separated by pipes
        let mut commands = input.trim().split(" | ").peekable();
        let mut previous_command: Option<Child> = None;
        let mut first_launched = true;

        // get the terminal settings so we can restore them later
        let shell_terminal = STDIN_FILENO;
        let mut shell_tmodes = libc::termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_cc: [0; 32],
            c_ispeed: 0,
            c_ospeed: 0,
            c_line: 0,
        };

        unsafe {
            libc::tcgetattr(shell_terminal, &mut shell_tmodes as *mut libc::termios);
        }

        // loop through each command
        while let Some(command) = commands.next() {

            // split the command into command and arguments
            let mut parts = command.trim().split_whitespace();
            let command = parts.next().unwrap_or_else(|| "");
            let args: Vec<&str> = parts.collect();

            match command {
                "" => {} // Do nothing on empty input
                "exit" => return, // Exit the shell

                "fg" => {
                    if let Some(child) = current_stopped {
                        let pid = child.id() as i32;
                        unsafe {
                            // libc::tcsetpgrp(shell_terminal, pid);
                            // libc::tcsetattr(shell_terminal, TCSADRAIN, &shell_tmodes);
                            libc::kill(pid, SIGCONT);
                            previous_command = Some(child);
                            current_stopped = None;
                            wait = true;
                            break;
                        }

                    // TODO DOESNT WORK when background process is stopped and put to foreground
                    } else if let Some(child) = backgound_tasks.lock().unwrap().pop() {
                        let pid = child.id() as i32;
                        unsafe {
                            libc::tcsetpgrp(STDIN_FILENO, pid);
                            // libc::tcsetattr(STDIN_FILENO, TCSADRAIN, &shell_tmodes);
                            previous_command = Some(child);
                            current_stopped = None;
                            wait = true;
                            libc::kill(pid, SIGCONT);
                            break;
                        }
                    }
                }

                // TODO DOESNT WORK, Permission denied (os error 13) when setpgid :(
                "bg" => {
                    if let Some(child) = current_stopped {
                        unsafe {
                            let pid: i32 = child.id() as i32;

                            if libc::setsid() < 0 {
                                eprintln!("setsid: {}", std::io::Error::last_os_error());
                                return;
                            }
                            if libc::setpgid(pid, pid) < 0 {
                                eprintln!("setpgid: {}", std::io::Error::last_os_error());
                                return;
                            }

                            if libc::kill(pid, libc::SIGCONT) < 0 {
                                eprintln!(
                                    "Error continuing process: {}",
                                    std::io::Error::last_os_error()
                                );
                                // Additional information for debugging
                                return;
                            }

                            backgound_tasks.lock().unwrap().push(child);
                        }
                        current_stopped = None;
                        wait = false;
                    }
                }
                
                "jobs" => {
                    for (i, child) in backgound_tasks.lock().unwrap().iter().enumerate() {
                        println!("[{}] {}", i, child.id());
                    }
                    backgound_tasks
                        .lock()
                        .unwrap()
                        .retain(|task| is_process_running(task.id() as i32));
                }
                "cd" => {
                    if args.is_empty() {
                        eprintln!("expected argument to \"cd\"");
                        continue;
                    }
                    let path = args.first().unwrap();
                    let root = Path::new(path);
                    if let Err(e) = std::env::set_current_dir(&root) {
                        eprintln!("{}", e);
                    }

                    previous_command = None;
                }
                mut command => {
                    let stdin = if command.contains('<') {
                        let c: Vec<&str> = command.split('<').collect();
                        command = c[0];
                        let file = c[1].trim();
                        Stdio::from(std::fs::File::open(file).unwrap())
                    } else {
                        previous_command.map_or(Stdio::inherit(), |output: Child| {
                            Stdio::from(output.stdout.unwrap())
                        })
                    };
                    let stdout = if command.contains('>') && !command.contains("2>") {
                        let c: Vec<&str> = command.split('>').collect();
                        command = c[0];
                        let file = c[1].trim();
                        Stdio::from(std::fs::File::create(file).unwrap())
                    } else {
                        if commands.peek().is_some() {
                            Stdio::piped()
                        } else {
                            Stdio::inherit()
                        }
                    };
                    let stderr = if command.contains("2>") {
                        let c: Vec<&str> = command.split("2>").collect();
                        command = c[0];
                        let file = c[1].trim();
                        Stdio::from(std::fs::File::create(file).unwrap())
                    } else {
                        Stdio::inherit()
                    };


                    unsafe {
                        let output: Result<Child, std::io::Error> = Command::new(command)
                            .args(args)
                            .stdin(stdin)
                            .stdout(stdout)
                            .stderr(stderr)
                            .pre_exec(move || {
                                if first_launched {
                                    if !wait {
                                        libc::setpgid(0, libc::getpid());
                                    }
                                    first_launched = false;
                                }
                                Ok(())
                            })
                            .spawn();
                        // let pid = output.as_ref().unwrap().id() as i32;
                        match output {
                            Ok(output) => {
                                previous_command = Some(output);
                                if !wait {
                                    let previous_command =
                                        std::mem::replace(&mut previous_command, None);
                                    backgound_tasks
                                        .lock()
                                        .unwrap()
                                        .push(previous_command.unwrap());
                                }
                            }
                            Err(e) => {
                                previous_command = None;
                                eprintln!("{}", e);
                            }
                        }
                    }
                }
            }
        }
        if let Some(final_command) = previous_command {
            // block until the final command has finished
            if wait {
                unsafe {
                    libc::setsid();
                    let fd = 0;
                    let child_pgrp = libc::tcgetpgrp(fd);
                    libc::tcsetpgrp(fd, child_pgrp);

                    // Wait for the child process to change state
                    let mut status = 0;
                    let wpid = final_command.id() as i32;
                    libc::waitpid(wpid, &mut status as *mut i32, WUNTRACED);
                    // if WIFEXITED(status) {
                    //     print!("0");
                    //     print!("Child process exited with status {}\n", WEXITSTATUS(status));
                    // } else if WIFSIGNALED(status) {
                    //     print!("Child process terminated by signal {}\n", WTERMSIG(status));
                    // } else if WIFSTOPPED(status) {
                    //     print!("Child process stopped by signal {}\n", WSTOPSIG(status));
                    //     current_stopped = Some(final_command);
                    // } else if WIFCONTINUED(status) {
                    //     print!("Child process continued\n");
                    // }
                    // libc::tcsetpgrp(shell_terminal, libc::getpid());
                    // print!("3");

                    libc::tcsetattr(shell_terminal, TCSADRAIN, &shell_tmodes);
                    let og_pgrep = libc::tcgetpgrp(shell_terminal);
                    libc::tcsetpgrp(shell_terminal, og_pgrep);
                }
            }
        }
    }
}
