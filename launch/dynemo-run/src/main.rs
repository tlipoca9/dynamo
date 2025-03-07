// SPDX-FileCopyrightText: Copyright (c) 2024-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::env;

use clap::Parser;

use dynemo_run::{Input, Output};
use dynemo_runtime::logging;

const HELP: &str = r#"
dynemo-run is a single binary that wires together the various inputs (http, text, network) and workers (network, engine), that runs the services. It is the simplest way to use dynemo locally.

Example:
- cargo build --release --features mistralrs,cuda
- cd target/release
- ./dynemo-run hf_checkouts/Llama-3.2-3B-Instruct/
- OR: ./dynemo-run Llama-3.2-1B-Instruct-Q4_K_M.gguf

"#;

const DEFAULT_IN: Input = Input::Text;

#[cfg(feature = "mistralrs")]
const DEFAULT_OUT: Output = Output::MistralRs;

#[cfg(not(feature = "mistralrs"))]
const DEFAULT_OUT: Output = Output::EchoFull;

const ZMQ_SOCKET_PREFIX: &str = "dyn";

const USAGE: &str = "USAGE: dynemo-run in=[http|text|dyn://<path>|none] out=[mistralrs|sglang|llamacpp|vllm|trtllm|echo_full|echo_core|pystr:<engine.py>] [--http-port 8080] [--model-path <path>] [--model-name <served-model-name>] [--model-config <hf-repo>] [--tensor-parallel-size=1] [--num-nodes=1] [--node-rank=0] [--leader-addr=127.0.0.1:9876] [--base-gpu-id=0]";

fn main() -> anyhow::Result<()> {
    logging::init();

    // Call sub-processes before starting the Runtime machinery
    // For anything except sub-process starting try_parse_from will error.
    if let Ok(flags) = dynemo_run::Flags::try_parse_from(env::args()) {
        #[allow(unused_variables)]
        if let Some(sglang_flags) = flags.internal_sglang_process {
            let Some(model_path) = flags.model_path_flag.as_ref() else {
                anyhow::bail!("sglang subprocess requires --model-path");
            };
            if !model_path.is_dir() {
                anyhow::bail!("sglang subprocess requires model path to be a directory containing the safetensors files");
            }
            if cfg!(feature = "sglang") {
                #[cfg(feature = "sglang")]
                {
                    use dynemo_llm::engines::sglang;
                    let gpu_config = sglang::MultiGPUConfig {
                        tp_size: flags.tensor_parallel_size,
                        tp_rank: sglang_flags.tp_rank,
                        gpu_id: sglang_flags.gpu_id,
                    };
                    let node_config = dynemo_llm::engines::MultiNodeConfig {
                        num_nodes: flags.num_nodes,
                        node_rank: flags.node_rank,
                        leader_addr: flags.leader_addr.unwrap_or_default(),
                    };
                    return sglang::run_subprocess(
                        ZMQ_SOCKET_PREFIX,
                        model_path,
                        sglang_flags.pipe_fd as std::os::fd::RawFd,
                        node_config,
                        gpu_config,
                    );
                }
            } else {
                panic!("Rebuild with --features=sglang");
            }
        }

        #[allow(unused_variables)]
        if flags.internal_vllm_process {
            let Some(model_path) = flags.model_path_flag else {
                anyhow::bail!("vllm subprocess requires --model-path flag");
            };
            let Some(model_config) = flags.model_config else {
                anyhow::bail!("vllm subprocess requires --model-config");
            };
            if !model_config.is_dir() {
                anyhow::bail!("vllm subprocess requires model config path to be a directory containing tokenizer.json, config.json, etc");
            }
            if cfg!(feature = "vllm") {
                #[cfg(feature = "vllm")]
                {
                    use dynemo_llm::engines::vllm;
                    let node_config = dynemo_llm::engines::MultiNodeConfig {
                        num_nodes: flags.num_nodes,
                        node_rank: flags.node_rank,
                        leader_addr: flags.leader_addr.unwrap_or_default(),
                    };
                    return vllm::run_subprocess(
                        ZMQ_SOCKET_PREFIX,
                        &model_config,
                        &model_path,
                        node_config,
                        flags.tensor_parallel_size,
                    );
                }
            } else {
                panic!("Rebuild with --features=vllm");
            }
        }
    }

    // max_worker_threads and max_blocking_threads from env vars or config file.
    let rt_config = dynemo_runtime::RuntimeConfig::from_settings()?;

    // One per process. Wraps a Runtime with holds two tokio runtimes.
    let worker = dynemo_runtime::Worker::from_config(rt_config)?;

    worker.execute(wrapper)
}

async fn wrapper(runtime: dynemo_runtime::Runtime) -> anyhow::Result<()> {
    let mut in_opt = None;
    let mut out_opt = None;
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("{USAGE}");
        println!("{HELP}");
        return Ok(());
    }
    for arg in env::args().skip(1).take(2) {
        let Some((in_out, val)) = arg.split_once('=') else {
            // Probably we're defaulting in and/or out, and this is a flag
            continue;
        };
        match in_out {
            "in" => {
                in_opt = Some(val.try_into()?);
            }
            "out" => {
                out_opt = Some(val.try_into()?);
            }
            _ => {
                anyhow::bail!("Invalid argument, must start with 'in' or 'out. {USAGE}");
            }
        }
    }
    let mut non_flag_params = 1; // binary name
    let in_opt = match in_opt {
        Some(x) => {
            non_flag_params += 1;
            x
        }
        None => DEFAULT_IN,
    };
    let out_opt = match out_opt {
        Some(x) => {
            non_flag_params += 1;
            x
        }
        None => DEFAULT_OUT,
    };

    // Clap skips the first argument expecting it to be the binary name, so add it back
    // Note `--model-path` has index=1 (in lib.rs) so that doesn't need a flag.
    let flags = dynemo_run::Flags::try_parse_from(
        ["dynemo-run".to_string()]
            .into_iter()
            .chain(env::args().skip(non_flag_params)),
    )?;

    dynemo_run::run(
        runtime,
        in_opt,
        out_opt,
        flags,
        Some(ZMQ_SOCKET_PREFIX.to_string()),
    )
    .await
}
