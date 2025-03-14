# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

"""Launch benchmark for a particular commit on cloud infrastructure, using bin/scratch"""

import argparse
import base64
import csv
import itertools
import os
import shlex
import sys
import time
from datetime import timedelta
from typing import List, NamedTuple, Optional, Union, cast

import boto3

from materialize import git, scratch, spawn, util
from materialize.cli.scratch import (
    DEFAULT_INSTPROF_NAME,
    DEFAULT_SG_ID,
    DEFAULT_SUBNET_ID,
    check_required_vars,
)
from materialize.scratch import print_instances


# This is duplicated with the one in cli/scratch.
# TODO - factor it out.
def main() -> None:
    os.chdir(os.environ["MZ_ROOT"])
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="subcommand", required=True)
    for cmd_name, configure, run in [
        ("start", configure_start, start),
        ("check", configure_check, check),
        #        ("mine", mine.configure_parser, mine.run),
        #        ("destroy", destroy.configure_parser, destroy.run),
    ]:
        s = subparsers.add_parser(cmd_name)
        configure(s)
        s.set_defaults(run=run)

    args = parser.parse_args()
    args.run(args)


def configure_start(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--profile",
        choices=["basic", "confluent"],
        type=str,
        required=True,
        help="Predefined set of machines to use in the cluster. 'basic' is only the Materialize instance; 'confluent' also includes a machine running the Kafka, Schema Registry, etc.",
    )
    parser.add_argument(
        "--trials",
        "-n",
        type=int,
        default=1,
        help="The number of trials to run per git rev",
    )
    parser.add_argument(
        "--revs",
        type=str,
        default="HEAD",
        help="Comma-separated list of git revs to benchmark",
    )
    parser.add_argument(
        "bench_script",
        type=str,
        nargs=argparse.REMAINDER,
        help="Benchmark script (and optional arguments)",
    )


class BenchSuccessResult(NamedTuple):
    stdout: str


class BenchFailureLogs(NamedTuple):
    stdout: str
    stderr: str


def configure_check(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("bench_id", type=str, nargs=1)


BUCKET = "mz-cloudbench"


def try_get_object(key: str) -> Optional[str]:
    client = boto3.client("s3")
    try:
        result = client.get_object(Bucket=BUCKET, Key=key)
        return result["Body"].read().decode("utf-8")
    except client.exceptions.NoSuchKey:
        return None


def check(ns: argparse.Namespace) -> None:
    check_required_vars()
    bench_id = ns.bench_id[0]

    manifest = (
        boto3.client("s3")
        .get_object(Bucket=BUCKET, Key=f"{bench_id}/MANIFEST")["Body"]
        .read()
        .decode("utf-8")
        .strip()
    )
    insts = manifest.split("\n")
    if not insts:
        raise RuntimeError(f"No instances found for bench ID {bench_id}")
    results: List[Optional[Union[BenchSuccessResult, BenchFailureLogs]]] = [
        None for _ in insts
    ]
    not_done = list(range(len(results)))
    while not_done:
        for i in not_done:
            maybe_result = try_get_object(f"{bench_id}/{insts[i]}.csv")
            if maybe_result is None:
                maybe_out = try_get_object(f"{bench_id}/{insts[i]}-FAILURE.out")
                maybe_err = try_get_object(f"{bench_id}/{insts[i]}-FAILURE.err")
                if (maybe_out is None) or (maybe_err is None):
                    continue
                results[i] = BenchFailureLogs(stdout=maybe_out, stderr=maybe_err)
            else:
                results[i] = BenchSuccessResult(stdout=maybe_result)

        not_done = [i for i in not_done if not results[i]]
        if not_done:
            print("Benchmark not done; waiting 60 seconds", file=sys.stderr)
            time.sleep(60)
    for r in results:
        assert isinstance(r, BenchSuccessResult) or isinstance(r, BenchFailureLogs)
    done_results = cast(List[Union[BenchFailureLogs, BenchSuccessResult]], results)
    failed = [
        (i, r) for i, r in enumerate(done_results) if isinstance(r, BenchFailureLogs)
    ]
    if failed:
        for i, f in failed:
            print(
                f"Run of instance {insts[i]} failed, stdout:\n{f.stdout}stderr:\n{f.stderr}",
                file=sys.stderr,
            )
        raise RuntimeError(f"{len(failed)} runs FAILED!")
    good_results = cast(List[BenchSuccessResult], done_results)
    readers = [
        csv.DictReader(f"{line}\n" for line in r.stdout.split("\n"))
        for r in good_results
    ]
    csv_results = ((d.values() for d in r) for r in readers)
    for r in readers:
        assert isinstance(r.fieldnames, list)
        for fn in r.fieldnames:
            assert isinstance(fn, str)
    headers = set(tuple(cast(List[str], r.fieldnames)) for r in readers)
    if len(headers) > 1:
        raise RuntimeError("Mismatched headers")
    w = csv.writer(sys.stdout)
    w.writerow(
        cast(List[str], readers[0].fieldnames) + ["InstanceIndex", "Rev", "Trial"]
    )
    for inst, r in zip(insts, csv_results):
        components = inst.split("-")
        for i, entry in enumerate(r):
            w.writerow(itertools.chain(entry, (components[0], components[1], i)))


def start(ns: argparse.Namespace) -> None:
    check_required_vars()

    revs = ns.revs.split(",")

    clusters = list(
        itertools.product(range(ns.trials), (git.rev_parse(rev) for rev in revs))
    )

    bench_script = ns.bench_script
    script_name = bench_script[0]
    script_args = " ".join((shlex.quote(arg) for arg in bench_script[1:]))

    # zip up the `misc` repository, for shipment to the remote machine
    os.chdir("misc/python")
    spawn.runv(["python3", "./setup.py", "sdist"])

    with open("./dist/materialize-0.0.0.tar.gz", "rb") as f:
        pkg_data = f.read()
    os.chdir(os.environ["MZ_ROOT"])

    mz_launch_script = f"""echo {shlex.quote(base64.b64encode(pkg_data).decode('utf-8'))} | base64 -d > mz.tar.gz
python3 -m venv /tmp/mzenv >&2
. /tmp/mzenv/bin/activate >&2
python3 -m pip install --upgrade pip >&2
pip3 install ./mz.tar.gz[dev] >&2
MZ_ROOT=/home/ubuntu/materialize python3 -u -m {script_name} {script_args}
result=$?
echo $result > ~/bench_exit_code
if [ $result -eq 0 ]; then
    aws s3 cp - s3://{BUCKET}/$MZ_CB_BENCH_ID/$MZ_CB_CLUSTER_ID.csv < ~/mzscratch-startup.out >&2
else
    aws s3 cp - s3://{BUCKET}/$MZ_CB_BENCH_ID/$MZ_CB_CLUSTER_ID-FAILURE.out < ~/mzscratch-startup.out >&2
    aws s3 cp - s3://{BUCKET}/$MZ_CB_BENCH_ID/$MZ_CB_CLUSTER_ID-FAILURE.err < ~/mzscratch-startup.err
fi
sudo shutdown -h now # save some money
"""

    if ns.profile == "basic":
        descs = [
            scratch.MachineDesc(
                name="materialized",
                launch_script=mz_launch_script,
                instance_type="r5a.4xlarge",
                ami="ami-0b29b6e62f2343b46",
                tags={},
                size_gb=64,
            ),
        ]
    elif ns.profile == "confluent":
        confluent_launch_script = f"""bin/mzcompose --mz-find load-tests up"""
        descs = [
            scratch.MachineDesc(
                name="materialized",
                launch_script=mz_launch_script,
                instance_type="r5a.4xlarge",
                ami="ami-0b29b6e62f2343b46",
                tags={},
                size_gb=64,
            ),
            scratch.MachineDesc(
                name="confluent",
                launch_script=confluent_launch_script,
                instance_type="r5a.4xlarge",
                ami="ami-0b29b6e62f2343b46",
                tags={},
                size_gb=1000,
                checkout=False,
            ),
        ]
    else:
        raise RuntimeError(f"Profile {ns.profile} is not implemented yet")

    bench_id = util.nonce(8)

    manifest_bytes = "".join(f"{i}-{rev}\n" for i, rev in clusters).encode("utf-8")
    boto3.client("s3").put_object(
        Body=manifest_bytes, Bucket="mz-cloudbench", Key=f"{bench_id}/MANIFEST"
    )

    # TODO - Do these in parallel
    launched = []
    for (i, rev) in clusters:
        launched += scratch.launch_cluster(
            descs=descs,
            nonce=f"{bench_id}-{i}-{rev}",
            subnet_id=DEFAULT_SUBNET_ID,
            security_group_id=DEFAULT_SG_ID,
            instance_profile=DEFAULT_INSTPROF_NAME,
            key_name=None,
            extra_tags={
                "bench_id": bench_id,
                "bench_rev": rev,
                "bench_i": str(i),
                "LaunchedBy": scratch.whoami(),
            },
            extra_env={"MZ_CB_BENCH_ID": bench_id, "MZ_CB_CLUSTER_ID": f"{i}-{rev}"},
            delete_after=scratch.now_plus(timedelta(days=1)),
            git_rev=rev,
        )

    print("Launched instances:")
    print_instances(launched, format="table")  # todo
    print(
        f"""Launched cloud bench with ID {bench_id}.
To wait for results, run: bin/cloudbench check {bench_id}"""
    )


if __name__ == "__main__":
    main()
