use std::path::PathBuf;

use std::net::{SocketAddr, TcpListener};

use crossbeam_channel::{unbounded, Receiver, Sender};
use std::thread;

use anyhow::{anyhow, Result};

use log::{debug, error, info, trace, warn};

use debugserver_types::{
    Breakpoint, Capabilities, ContinueResponseBody, DisconnectArguments, EvaluateResponseBody,
    Event, InitializeRequestArguments, InitializedEvent, ProtocolMessage, Request, Response,
    SetBreakpointsArguments, SetBreakpointsResponseBody, StackTraceResponseBody, Thread,
    ThreadsResponseBody,
};

use std::io::{BufRead, BufReader, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use serde_json::{from_slice, from_value, json, to_vec};

use super::{
    commands::{
        debug_event::DebugEvent, debug_request::DebugRequest, debug_response::DebugResponse,
        Command,
    },
    debugger::DebugHandler,
};

use probe_rs::HaltReason;

pub fn start_tcp_server(port: u16) -> Result<()> {
    info!("Starting debug-adapter server on port: {}", port);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr)?;

    loop {
        let (socket, addr) = listener.accept()?;
        socket.set_nonblocking(true)?;
        info!("Accepted connection from {}", addr);

        let reader = BufReader::new(socket.try_clone()?);
        let writer = socket;

        start_debugger_and_adapter(reader, writer)?;
    }
}

fn start_debugger_and_adapter<R: Read, W: Write>(reader: BufReader<R>, writer: W) -> Result<()> {
    let (debugger_sender, debug_adapter_receiver): (Sender<Command>, Receiver<Command>) =
        unbounded();
    let (debug_adapter_sender, debugger_receiver): (Sender<DebugRequest>, Receiver<DebugRequest>) =
        unbounded();

    let debugger_th = thread::spawn(move || {
        let mut debugger = DebugHandler::new_default();
        match debugger.run(debugger_sender, debugger_receiver) {
            Ok(_) => (),
            Err(err) => warn!("DebugThread stoped because of error: {:?}", err),
        };
        info!("DebugThread stoped");
    });

    let mut da = DebugAdapter::new(reader, writer, debug_adapter_sender, debug_adapter_receiver);
    match da.run() {
        Ok(_) => (),
        Err(err) => warn!("DebugAdapterThread stoped because of error: {:?}", err),
    };
    info!("DebugAdapterThread stoped");
    debugger_th.join().expect("oops! the child thread panicked");

    Ok(())
}

pub struct DebugAdapter<R: Read, W: Write> {
    seq: i64,
    reader: BufReader<R>,
    writer: W,
    sender: Sender<DebugRequest>,
    receiver: Receiver<Command>,
}

impl<R: Read, W: Write> DebugAdapter<R, W> {
    pub fn new(
        reader: BufReader<R>,
        writer: W,
        sender: Sender<DebugRequest>,
        receiver: Receiver<Command>,
    ) -> DebugAdapter<R, W> {
        DebugAdapter {
            seq: 0,
            reader: reader,
            writer: writer,
            sender: sender,
            receiver: receiver,
        }
    }

    fn init(&mut self) -> Result<()> {
        let message = {
            let res;
            loop {
                match read_dap_msg(&mut self.reader) {
                    Ok(val) => {
                        res = val;
                        break;
                    }
                    Err(_) => continue,
                };
            }
            res
        };

        let request = verify_init_msg(message)?;

        let capabilities = Capabilities {
            supports_configuration_done_request: Some(true), // Supports config after init request
            //            supports_data_breakpoints:              Some(true),
            //        supportsCancelRequest:                  Some(true),
            ..Default::default()
        };

        let resp = Response {
            body: Some(json!(capabilities)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&resp)?, self.seq)?;

        self.seq = send_data(
            &mut self.writer,
            &to_vec(&InitializedEvent {
                seq: self.seq,
                body: None,
                type_: "event".to_owned(),
                event: "initialized".to_owned(),
            })?,
            self.seq,
        )?;

        Ok(())
    }

    pub fn run(&mut self) -> Result<()> {
        self.init()?;
        loop {
            // Check for events
            match self.receiver.try_recv() {
                Ok(Command::Event(event)) => self.handle_event_command(event)?,
                Ok(_) => error!("Unreachable"),
                Err(_) => (),
            };

            // Check for DAP messages
            let message = match read_dap_msg(&mut self.reader) {
                Ok(val) => val,
                Err(_err) => continue,
            };

            let exit = self.handle_dap_message(message)?;

            // Exit the debug session
            if exit {
                return Ok(());
            }
        }
    }

    fn handle_dap_message(&mut self, message: DebugAdapterMessage) -> Result<bool> {
        match message {
            DebugAdapterMessage::Request(req) => self.handle_dap_request(req),
            DebugAdapterMessage::Response(_resp) => {
                error!("Unimplemented");
                return Err(anyhow!("Unimplemented"));
            }
            DebugAdapterMessage::Event(_event) => {
                error!("Unimplemented");
                return Err(anyhow!("Unimplemented"));
            }
        }
    }

    fn handle_dap_request(&mut self, request: Request) -> Result<bool> {
        let result = match request.command.as_ref() {
            "launch" => self.handle_launch_dap_request(&request),
            "attach" => self.handle_attach_dap_request(&request),
            "setBreakpoints" => self.handle_set_breakpoints_dap_request(&request),
            "threads" => self.handle_threads_dap_request(&request),
            //          //  "setDataBreakpoints"        => Ok(()), // TODO
            //          //  "setExceptionBreakpoints"   => Ok(()), // TODO
            "configurationDone" => self.handle_configuration_done_dap_request(&request),
            "pause" => self.handle_pause_dap_request(&request),
            "stackTrace" => self.handle_stack_trace_dap_request(&request),
            "disconnect" => self.handle_disconnect_dap_request(&request),
            "continue" => self.handle_continue_dap_request(&request),
            "scopes" => self.handle_scopes_dap_request(&request),
            "source" => {
                error!("Unimpleemted");
                Ok(false) // NOTE: Return Error maybe
            }
            "variables" => self.handle_variables_dap_request(&request),
            "next" => self.handle_next_dap_request(&request),
            "stepIn" => self.handle_next_dap_request(&request), // TODO
            "stepOut" => self.handle_next_dap_request(&request), // TODO
            "evaluate" => self.handle_evaluate_dap_request(&request),
            _ => {
                error!("command: {}", request.command);
                Ok(false) // NOTE: Return Error maybe
            }
        };

        match result {
            Ok(v) => Ok(v),
            Err(err) => {
                warn!("Error when handeling DAP message: {}", err.to_string());
                let response = Response {
                    body: None,
                    command: request.command.clone(),
                    message: Some(err.to_string()),
                    request_seq: request.seq,
                    seq: self.seq,
                    success: false,
                    type_: "response".to_string(),
                };

                self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

                Ok(false)
            }
        }
    }

    fn handle_event_command(&mut self, event: DebugEvent) -> Result<()> {
        debug!("event {:?}", event);
        match event {
            DebugEvent::Halted {
                pc: _,
                reason,
                hit_breakpoint_ids,
            } => {
                let (reason_str, description) = match reason {
                    HaltReason::Breakpoint => (
                        "breakpoint".to_owned(),
                        Some("Target stopped due to breakpoint.".to_owned()),
                    ),
                    _ => (format!("{:?}", reason), None),
                };
                let body = StoppedEventBody {
                    reason: reason_str,
                    description: description,
                    thread_id: Some(0),
                    preserve_focus_hint: None,
                    text: None,
                    all_threads_stopped: None,
                    hit_breakpoint_ids: hit_breakpoint_ids,
                };

                self.seq = send_data(
                    &mut self.writer,
                    &to_vec(&Event {
                        body: Some(json!(body)),
                        event: "stopped".to_owned(),
                        seq: self.seq,
                        type_: "event".to_owned(),
                    })?,
                    self.seq,
                )?;
            }
        };

        Ok(())
    }

    fn handle_launch_dap_request(&mut self, _request: &Request) -> Result<bool> {
        error!("Unimplemented");
        Ok(false) // NOTE: return error maybe
    }

    fn handle_attach_dap_request(&mut self, request: &Request) -> Result<bool> {
        let args: AttachRequestArguments = get_arguments(&request)?;
        debug!("attach args: {:#?}", args);
        info!("program: {:?}", args.program);

        // Set binary path
        let path = PathBuf::from(args.program);
        self.sender.send(DebugRequest::SetBinary { path: path })?;

        // Get DebugResponse
        let _ack = self.retrieve_response()?;

        // Set chip
        self.sender.send(DebugRequest::SetChip {
            chip: args.chip.clone(),
        })?;

        // Get DebugResponse
        let _ack = self.retrieve_response()?;

        match args.cwd {
            Some(cwd) => {
                // Set cwd
                self.sender.send(DebugRequest::SetCWD { cwd: cwd })?;

                // Get DebugResponse
                let _ack = self.retrieve_response()?;
            }
            None => (),
        };

        // Flash and attach or just attach to the core
        match args.flash {
            Some(true) => {
                // Flash to chip
                self.sender.send(DebugRequest::Flash {
                    reset_and_halt: match args.halt_after_reset {
                        Some(val) => val,
                        None => false,
                    },
                })?;

                // Get Flash DebugResponse
                let _ack = self.retrieve_response()?;
            }
            _ => {
                // Attach to chip
                self.sender.send(DebugRequest::Attach {
                    reset: match args.reset {
                        Some(val) => val,
                        None => false,
                    },
                    reset_and_halt: match args.halt_after_reset {
                        Some(val) => val,
                        None => false,
                    },
                })?;

                // Get Attach DebugResponse
                let _ack = self.retrieve_response()?;
            }
        };

        let response = Response {
            body: None,
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: request.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_configuration_done_dap_request(&mut self, request: &Request) -> Result<bool> {
        let response = Response {
            body: None,
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_threads_dap_request(&mut self, request: &Request) -> Result<bool> {
        let body = ThreadsResponseBody {
            threads: vec![Thread {
                id: 0,
                name: "Main Thread".to_string(),
            }],
        };

        let response = Response {
            body: Some(json!(body)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_pause_dap_request(&mut self, request: &Request) -> Result<bool> {
        // Send halt DebugRequest
        self.sender.send(DebugRequest::Halt)?;

        // Get halt DebugResponse
        let _ack = self.retrieve_response()?;

        let response = Response {
            body: None,
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };
        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_stack_trace_dap_request(&mut self, request: &Request) -> Result<bool> {
        let args: debugserver_types::StackTraceArguments = get_arguments(&request)?;
        debug!("args: {:?}", args);

        // Get DAP stack frames
        self.sender.send(DebugRequest::DAPStackFrames)?;

        // Get stack trace DebugResponse
        let ack = self.retrieve_response()?;
        let stack_frames = match ack {
            DebugResponse::DAPStackFrames { stack_frames } => stack_frames,
            _ => {
                error!("Unreachable");
                return Err(anyhow!("Unreachable"));
            }
        };

        let total_frames = stack_frames.len() as i64;
        let body = StackTraceResponseBody {
            stack_frames: stack_frames,
            total_frames: Some(total_frames),
        };

        let response = Response {
            body: Some(json!(body)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_scopes_dap_request(&mut self, request: &Request) -> Result<bool> {
        let args: debugserver_types::ScopesArguments = get_arguments(&request)?;
        debug!("args: {:?}", args);

        // Get stack trace
        self.sender.send(DebugRequest::DAPScopes {
            frame_id: args.frame_id,
        })?;

        // Get stack trace DebugResponse
        let ack = self.retrieve_response()?;
        let scopes = match ack {
            DebugResponse::DAPScopes { scopes } => scopes,
            _ => {
                error!("Unreachable");
                vec![]
            }
        };

        let body = debugserver_types::ScopesResponseBody { scopes: scopes };

        let response = Response {
            body: Some(json!(body)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_variables_dap_request(&mut self, request: &Request) -> Result<bool> {
        let args: debugserver_types::VariablesArguments = get_arguments(&request)?;
        debug!("args: {:?}", args);

        // Get stack trace
        self.sender.send(DebugRequest::DAPVariables {
            id: args.variables_reference,
        })?;

        // Get stack trace DebugResponse
        let ack = self.retrieve_response()?;
        let vars = match ack {
            DebugResponse::DAPVariables { variables } => variables,
            _ => {
                error!("Unreachable");
                return Err(anyhow!("Unreachable"));
            }
        };

        // Parse variables
        let mut variables = vec![];

        for var in &vars {
            let (indexed_variables, named_variables) = var.get_num_diff_children();
            variables.push(debugserver_types::Variable {
                evaluate_name: None, //Option<String>,
                indexed_variables: Some(indexed_variables),
                name: match &var.name {
                    Some(name) => name.clone(),
                    None => "<unknown>".to_string(),
                },
                named_variables: Some(named_variables),
                presentation_hint: None,
                type_: Some(var.type_.clone()),
                value: var.value_to_string(),
                variables_reference: var.id, // i64,
            });
        }

        let body = debugserver_types::VariablesResponseBody {
            variables: variables,
        };

        let response = Response {
            body: Some(json!(body)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_continue_dap_request(&mut self, request: &Request) -> Result<bool> {
        // Send continue DebugRequest
        self.sender.send(DebugRequest::Continue)?;

        // Get Continue DebugResponse
        let _ack = self.retrieve_response()?;

        let body = ContinueResponseBody {
            all_threads_continued: Some(true),
        };

        let response = Response {
            body: Some(json!(body)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_disconnect_dap_request(&mut self, request: &Request) -> Result<bool> {
        let args: DisconnectArguments = get_arguments(&request)?;
        debug!("args: {:?}", args);
        // TODO: Stop the debuggee, if conditions are meet

        // Send Exit DebugRequest
        self.sender.send(DebugRequest::Exit)?;

        // Get Exit DebugResponse
        let _ack = self.retrieve_response()?;

        let response = Response {
            body: None,
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(true)
    }

    fn handle_next_dap_request(&mut self, request: &Request) -> Result<bool> {
        // Send Step DebugRequest
        self.sender.send(DebugRequest::Step)?;

        // Get Step DebugResponse
        let _ack = self.retrieve_response()?;

        let response = Response {
            body: None,
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_evaluate_dap_request(&mut self, request: &Request) -> Result<bool> {
        let body = EvaluateResponseBody {
            result: "This feature is not yet implemented".to_owned(),
            variables_reference: 0.0,
            type_: None,
            indexed_variables: None,
            named_variables: None,
            presentation_hint: None,
        };

        let response = Response {
            body: Some(json!(body)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn handle_set_breakpoints_dap_request(&mut self, request: &Request) -> Result<bool> {
        let args: SetBreakpointsArguments = get_arguments(request)?;
        debug!("args: {:#?}", args);

        let source_breakpoints = match args.breakpoints {
            Some(bkpts) => bkpts,
            None => vec![],
        };

        let breakpoints: Vec<Breakpoint> = match args.source.path.clone() {
            Some(path) => {
                // Send SetBreakpoints DebugRequest
                self.sender.send(DebugRequest::SetBreakpoints {
                    source_file: path,
                    source_breakpoints: source_breakpoints,
                    source: Some(args.source.clone()),
                })?;

                // Get SetBreakpoints DebugResponse
                let ack = self.retrieve_response()?;
                let breakpoints = match ack {
                    DebugResponse::SetBreakpoints { breakpoints } => breakpoints,
                    _ => {
                        error!("Unreachable: {:#?}", ack);
                        vec![]
                    }
                };
                breakpoints
            }
            None => vec![],
        };

        let body = SetBreakpointsResponseBody {
            breakpoints: breakpoints,
        };

        let response = Response {
            body: Some(json!(body)),
            command: request.command.clone(),
            message: None,
            request_seq: request.seq,
            seq: self.seq,
            success: true,
            type_: "response".to_string(),
        };

        self.seq = send_data(&mut self.writer, &to_vec(&response)?, self.seq)?;

        Ok(false)
    }

    fn retrieve_response(&mut self) -> Result<DebugResponse> {
        // Get DebugResponse
        loop {
            let command = self.receiver.recv()?;
            match command {
                Command::Response(response) => {
                    if let DebugResponse::Error { message } = response {
                        return Err(anyhow!("{}", message));
                    }
                    return Ok(response);
                }
                Command::Event(event) => self.handle_event_command(event)?,
                _ => {
                    error!("Unreachable");
                    return Err(anyhow!("Unreachable"));
                }
            };
        }
    }
}

fn verify_init_msg(message: DebugAdapterMessage) -> Result<Request> {
    match message {
        DebugAdapterMessage::Request(req) => {
            if req.command != "initialize" {
                return Err(anyhow!(
                    "Error: Expected command initialize got {}",
                    req.command
                ));
            }

            let arguments: InitializeRequestArguments = get_arguments(&req)?;
            debug!(
                "Initialization request from client '{}'",
                arguments.client_name.unwrap_or("<unknown>".to_owned())
            );
            Ok(req)
        }

        _ => Err(anyhow!("Error: initial message should be of type request")),
    }
}

fn read_dap_msg<R: Read>(reader: &mut BufReader<R>) -> Result<DebugAdapterMessage, anyhow::Error> {
    let mut header = String::new();

    reader.read_line(&mut header)?;
    trace!("< {}", header.trim_end());

    // we should read an empty line here
    let mut buff = String::new();
    reader.read_line(&mut buff)?;

    let len = get_content_len(&header)
        .ok_or_else(|| anyhow!("Failed to read content length from header '{}'", header))?;

    let mut content = vec![0u8; len];
    let _bytes_read = reader.read(&mut content)?;

    // Extract protocol message
    let protocol_msg: ProtocolMessage = from_slice(&content)?;

    let msg = match protocol_msg.type_.as_ref() {
        "request" => DebugAdapterMessage::Request(from_slice(&content)?),
        "response" => DebugAdapterMessage::Response(from_slice(&content)?),
        "event" => DebugAdapterMessage::Event(from_slice(&content)?),
        other => return Err(anyhow!("Unknown message type: {}", other)),
    };

    trace!("< {:#?}", msg);
    Ok(msg)
}

fn get_content_len(header: &str) -> Option<usize> {
    let mut parts = header.trim_end().split_ascii_whitespace();

    // discard first part
    parts.next()?;
    parts.next()?.parse::<usize>().ok()
}

#[derive(Debug)]
pub enum DebugAdapterMessage {
    Request(Request),
    Response(Response),
    Event(Event),
}

pub fn get_arguments<T: DeserializeOwned>(req: &Request) -> Result<T> {
    let value = match req.arguments.as_ref() {
        Some(val) => val,
        None => {
            error!("Expacted arguments");
            return Err(anyhow!("Expected arguments"));
        }
    };
    from_value(value.to_owned()).map_err(|e| e.into())
}

pub fn send_data<W: Write>(writer: &mut W, raw_data: &[u8], seq: i64) -> Result<i64> {
    let resp_body = raw_data;

    let resp_header = format!("Content-Length: {}\r\n\r\n", resp_body.len());

    //trace!("> {}", resp_header.trim_end());
    trace!("> {}", std::str::from_utf8(resp_body)?);

    writer.write(resp_header.as_bytes())?;
    writer.write(resp_body)?;

    writer.flush()?;

    Ok(seq + 1)
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StoppedEventBody {
    pub all_threads_stopped: Option<bool>,
    pub description: Option<String>,
    pub preserve_focus_hint: Option<bool>,
    pub reason: String,
    pub text: Option<String>,
    pub thread_id: Option<i64>,
    pub hit_breakpoint_ids: Option<Vec<u32>>,
}

#[derive(Deserialize, Debug, Default)]
struct AttachRequestArguments {
    program: String,
    chip: String,
    cwd: Option<String>,
    reset: Option<bool>,
    halt_after_reset: Option<bool>,
    flash: Option<bool>,
}

#[derive(Deserialize, Debug, Default)]
struct LaunchRequestArguments {
    program: String,
    chip: String,
    cwd: Option<String>,
    reset: Option<bool>,
    no_debug: Option<bool>,
    halt_after_reset: Option<bool>,
}
