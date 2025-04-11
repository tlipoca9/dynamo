# SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
import asyncio
import logging

from common.base_engine import BaseTensorrtLLMEngine
from common.parser import parse_tensorrt_llm_args
from common.protocol import TRTLLMWorkerRequest
from common.utils import ServerType

from dynamo.sdk import async_on_start, dynamo_context, dynamo_endpoint, service
from dynamo.sdk.lib.config import ServiceConfig

logger = logging.getLogger(__name__)


@service(
    dynamo={
        "enabled": True,
        "namespace": "dynamo",
    },
    resources={"gpu": 1, "cpu": "10", "memory": "20Gi"},
    workers=1,
)
class TensorRTLLMPrefillWorker(BaseTensorrtLLMEngine):
    def __init__(self):
        logger.info("Initializing TensorRT-LLM Prefill Worker")
        class_name = self.__class__.__name__
        config = ServiceConfig.get_instance()
        config_args = config.as_args(class_name, prefix="")
        args, engine_config = parse_tensorrt_llm_args(config_args)
        worker_id = dynamo_context["endpoints"][0].lease_id()
        super().__init__(
            namespace_str="dynamo",
            component_str=class_name,
            worker_id=worker_id,
            engine_config=engine_config,
            remote_prefill=args.remote_prefill,
            min_workers=args.min_workers,
            disagg_config_file=args.llmapi_disaggregated_config,
            block_size=args.block_size,
            router=args.router,
            server_type=ServerType.CTX,
        )

    @async_on_start
    async def async_init(self):
        self._init_engine()
        if self._kv_metrics_publisher is not None:
            task = asyncio.create_task(self.create_metrics_publisher_endpoint())
            task.add_done_callback(
                lambda _: logger.info("metrics publisher endpoint created")
            )
        logger.info("TensorRT-LLM Prefill Worker initialized")

    async def create_metrics_publisher_endpoint(self):
        component = dynamo_context["component"]
        await self.kv_metrics_publisher.create_endpoint(component)

    @dynamo_endpoint()
    async def generate(self, request: TRTLLMWorkerRequest):
        async for response in super().generate(request):
            yield response
