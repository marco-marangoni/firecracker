#!/usr/bin/env python3
# Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""TEMPORARY: aarch64-only reproduction of the serial runtime-PM TX deferral."""

from common import BKPipeline

pipeline = BKPipeline(priority=2, timeout_in_minutes=45)
pipeline.per_instance["instances"] = ["m7g.metal"]
pipeline.per_instance["platforms"] = [("al2023", "linux_6.1")]
pipeline.build_group(
    "serial-pm-repro",
    pipeline.devtool_test(
        pytest_opts="-s integration_tests/functional/test_zz_serial_pm_repro.py integration_tests/functional/test_serial_io.py::test_serial_file_output",
    ),
)
print(pipeline.to_json())
