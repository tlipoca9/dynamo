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

use std::ffi::CStr;
use std::{path::Path, sync::Arc};

use dynemo_runtime::pipeline::error as pipeline_error;
pub use dynemo_runtime::{
    error,
    pipeline::{
        async_trait, AsyncEngine, AsyncEngineContextProvider, Data, ManyOut, ResponseStream,
        SingleIn,
    },
    protocols::annotated::Annotated,
    Error, Result,
};
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyDict};
use pyo3_async_runtimes::TaskLocals;
use pythonize::{depythonize, pythonize};
pub use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::oneshot::Sender;
use tokio_stream::{wrappers::ReceiverStream, StreamExt};

use crate::types::openai::chat_completions::OpenAIChatCompletionsStreamingEngine;

/// Python snippet to import a file as a module
const PY_IMPORT: &CStr = cr#"
import importlib.util
import sys

module_name = file_path.split("/")[-1].replace(".py", "")
spec = importlib.util.spec_from_file_location(module_name, file_path)

module = importlib.util.module_from_spec(spec)

sys.modules[module_name] = module
spec.loader.exec_module(module)
"#;

/// An engine that takes and returns strings, feeding them to a python written engine
pub async fn make_string_engine(
    py_file: &Path,
) -> pipeline_error::Result<OpenAIChatCompletionsStreamingEngine> {
    pyo3::prepare_freethreaded_python();

    let engine = PythonStringEngine::new(py_file).await?;
    let engine: OpenAIChatCompletionsStreamingEngine = Arc::new(engine);
    Ok(engine)
}

struct PythonStringEngine {
    _user_module: PyObject,
    generator: Arc<Py<PyAny>>,
    event_loop: Arc<Py<PyAny>>,
}

impl PythonStringEngine {
    async fn new(py_file: &Path) -> anyhow::Result<Self> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || run_asyncio(tx));
        let event_loop = rx.await?;

        let user_module = python_file_to_module(py_file)?;
        let generator = Python::with_gil(|py| user_module.getattr(py, "generate").unwrap());
        Ok(PythonStringEngine {
            _user_module: user_module,
            generator: Arc::new(generator),
            event_loop,
        })
    }
}

/// Start asyncio event loop and block on it forever
fn run_asyncio(tx: Sender<Arc<PyObject>>) {
    let event_loop: PyObject = Python::with_gil(|py| {
        let aio: PyObject = py.import("asyncio").unwrap().into();
        aio.call_method0(py, "new_event_loop").unwrap()
    });
    let event_loop = Arc::new(event_loop);
    let _ = tx.send(event_loop.clone());
    Python::with_gil(|py| {
        let _ = event_loop.call_method0(py, "run_forever");
    });
}

fn python_file_to_module(p: &Path) -> Result<PyObject> {
    let module: PyObject = Python::with_gil(|py| {
        let globals = [("file_path", p.display().to_string())]
            .into_py_dict(py)
            .unwrap();
        let locals = PyDict::new(py);
        py.run(PY_IMPORT, Some(&globals), Some(&locals)).unwrap();
        let module = locals.get_item("module").unwrap().unwrap();
        module.extract().unwrap()
    });
    Ok(module)
}

#[derive(Debug, thiserror::Error)]
enum ResponseProcessingError {
    #[error("python exception: {0}")]
    PythonException(String),

    #[error("deserialize error: {0}")]
    DeserializeError(String),

    #[error("gil offload error: {0}")]
    OffloadError(String),
}

#[async_trait]
impl<Req, Resp> AsyncEngine<SingleIn<Req>, ManyOut<Annotated<Resp>>, Error> for PythonStringEngine
where
    Req: Data + Serialize,
    Resp: Data + for<'de> Deserialize<'de>,
{
    async fn generate(&self, request: SingleIn<Req>) -> Result<ManyOut<Annotated<Resp>>, Error> {
        // Create a context
        let (request, context) = request.transfer(());
        let ctx = context.context();

        let id = context.id().to_string();
        tracing::trace!("processing request: {}", id);

        // Clone the PyObject to move into the thread

        // Create a channel to communicate between the Python thread and the Rust async context
        let (tx, rx) = mpsc::channel::<Annotated<Resp>>(128);

        let generator = self.generator.clone();
        let event_loop = self.event_loop.clone();

        // Acquiring the GIL is similar to acquiring a standard lock/mutex
        // Performing this in an tokio async task could block the thread for an undefined amount of time
        // To avoid this, we spawn a blocking task to acquire the GIL and perform the operations needed
        // while holding the GIL.
        //
        // Under low GIL contention, we wouldn't need to do this.
        // However, under high GIL contention, this can lead to significant performance degradation.
        //
        // Since we cannot predict the GIL contention, we will always use the blocking task and pay the
        // cost. The Python GIL is the gift that keeps on giving -- performance hits...
        let stream = tokio::task::spawn_blocking(move || {
            Python::with_gil(|py| {
                let py_request = pythonize(py, &request)?;
                let gen = generator.call1(py, (py_request,))?;
                let locals = TaskLocals::new(event_loop.bind(py).clone());
                pyo3_async_runtimes::tokio::into_stream_with_locals_v1(locals, gen.into_bound(py))
            })
        })
        .await??;

        let stream = Box::pin(stream);

        // process the stream
        // any error thrown in the stream will be caught and complete the processing task
        // errors are captured by a task that is watching the processing task
        // the error will be emitted as an annotated error
        let request_id = id.clone();

        tokio::spawn(async move {
            tracing::debug!(
                request_id,
                "starting task to process python async generator stream"
            );

            let mut stream = stream;
            let mut count = 0;

            while let Some(item) = stream.next().await {
                count += 1;
                tracing::trace!(
                    request_id,
                    "processing the {}th item from python async generator",
                    count
                );

                let mut done = false;

                let response = match process_item::<Resp>(item).await {
                    Ok(response) => response,
                    Err(e) => {
                        done = true;

                        let msg = match &e {
                            ResponseProcessingError::DeserializeError(e) => {
                                // tell the python async generator to stop generating
                                // right now, this is impossible as we are not passing the context to the python async generator
                                // todo: add task-local context to the python async generator
                                ctx.stop_generating();
                                let msg = format!("critical error: invalid response object from python async generator; application-logic-mismatch: {}", e);
                                tracing::error!(request_id, "{}", msg);
                                msg
                            }
                            ResponseProcessingError::PythonException(e) => {
                                let msg = format!("a python exception was caught while processing the async generator: {}", e);
                                tracing::warn!(request_id, "{}", msg);
                                msg
                            }
                            ResponseProcessingError::OffloadError(e) => {
                                let msg = format!("critical error: failed to offload the python async generator to a new thread: {}", e);
                                tracing::error!(request_id, "{}", msg);
                                msg
                            }
                        };

                        Annotated::from_error(msg)
                    }
                };

                if tx.send(response).await.is_err() {
                    tracing::trace!(
                        request_id,
                        "error forwarding annotated response to channel; channel is closed"
                    );
                    break;
                }

                if done {
                    tracing::debug!(
                        request_id,
                        "early termination of python async generator stream task"
                    );
                    break;
                }
            }

            tracing::debug!(
                request_id,
                "finished processing python async generator stream"
            );
        });

        let stream = ReceiverStream::new(rx);

        Ok(ResponseStream::new(Box::pin(stream), context.context()))
    }
}

async fn process_item<Resp>(
    item: Result<Py<PyAny>, PyErr>,
) -> Result<Annotated<Resp>, ResponseProcessingError>
where
    Resp: Data + for<'de> Deserialize<'de>,
{
    let item = item.map_err(|e| ResponseProcessingError::PythonException(e.to_string()))?;

    let response = tokio::task::spawn_blocking(move || {
        Python::with_gil(|py| depythonize::<Resp>(&item.into_bound(py)))
    })
    .await
    .map_err(|e| ResponseProcessingError::OffloadError(e.to_string()))?
    .map_err(|e| ResponseProcessingError::DeserializeError(e.to_string()))?;

    let response = Annotated::from_data(response);

    Ok(response)
}
