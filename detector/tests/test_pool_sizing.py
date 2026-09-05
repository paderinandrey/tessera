"""How many inferences this process may usefully run at once.

Three wrong answers were shipped before the right question was asked, and each
was wrong the same way — it described something other than what the process may
do:

- `os.cpu_count()` describes the **machine**. A container given two CPUs on a
  64-core host reads 64, builds 64 workers, and lets one document enqueue 32
  inferences onto two;
- `os.process_cpu_count()` describes the **affinity mask**. Right for a
  `cpuset`, still wrong for a *quota* — which is what `docker --cpus` and a
  Kubernetes CPU limit actually write. Confirmed in review of #63, in an
  environment with a two-CPU quota where it returned three;
- a floor of two workers describes **a machine with more than one CPU**. On a
  single-CPU deployment it starts two CPU-heavy inferences on one core.

None of the three can be tested by looking at the machine the tests run on,
which is why they survived. These drive the parser and the override directly.
"""

from __future__ import annotations

import pytest

import tessera_detector.ner as ner


def test_a_cgroup_v2_quota_is_read_as_cpus(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    quota = tmp_path / "cpu.max"
    monkeypatch.setattr(ner, "_CGROUP_V2_QUOTA", quota)
    monkeypatch.setattr(ner, "_CGROUP_V1_QUOTA", tmp_path / "absent")

    quota.write_text("200000 100000\n")
    assert ner._cgroup_cpu_quota() == 2.0
    quota.write_text("50000 100000\n")
    assert ner._cgroup_cpu_quota() == 0.5, "half a CPU is a real limit and rounds later"
    # "max" is how cgroup v2 spells no limit, and reading it as a number is how
    # a limitless container would get a pool sized from a parse error.
    quota.write_text("max 100000\n")
    assert ner._cgroup_cpu_quota() is None


def test_a_cgroup_v1_quota_is_read_when_v2_is_absent(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A deployment does not choose which cgroup version its kernel exposes.
    monkeypatch.setattr(ner, "_CGROUP_V2_QUOTA", tmp_path / "absent")
    v1_quota, v1_period = tmp_path / "quota", tmp_path / "period"
    monkeypatch.setattr(ner, "_CGROUP_V1_QUOTA", v1_quota)
    monkeypatch.setattr(ner, "_CGROUP_V1_PERIOD", v1_period)

    v1_quota.write_text("300000\n")
    v1_period.write_text("100000\n")
    assert ner._cgroup_cpu_quota() == 3.0
    # -1 is how cgroup v1 spells no limit.
    v1_quota.write_text("-1\n")
    assert ner._cgroup_cpu_quota() is None


def test_an_unreadable_cgroup_costs_the_default_and_not_an_exception(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # This runs at import. A kernel that spells its files differently must cost
    # the default sizing, never a service that will not start.
    for name in ("_CGROUP_V2_QUOTA", "_CGROUP_V1_QUOTA", "_CGROUP_V1_PERIOD"):
        monkeypatch.setattr(ner, name, tmp_path / "absent")
    assert ner._cgroup_cpu_quota() is None

    garbage = tmp_path / "cpu.max"
    monkeypatch.setattr(ner, "_CGROUP_V2_QUOTA", garbage)
    for content in ("", "nonsense", "200000", "abc def", "200000 0\n"):
        garbage.write_text(content)
        assert ner._cgroup_cpu_quota() is None, f"{content!r} produced a number"


def test_the_quota_wins_when_it_is_narrower(monkeypatch: pytest.MonkeyPatch) -> None:
    # The case that motivated all of this: an affinity mask wider than the
    # quota, which is every `docker --cpus` container.
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 64)
    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: 2.0)
    monkeypatch.delenv(ner._WORKERS_ENV, raising=False)
    assert ner._pool_size() == 2

    # And it does not win when it is wider: a quota above the affinity mask is
    # a promise the scheduler will not keep.
    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: 100.0)
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 4)
    assert ner._pool_size() == 4


def test_a_fractional_quota_still_leaves_one_worker(monkeypatch: pytest.MonkeyPatch) -> None:
    # `--cpus=0.5` is a real thing to write. Truncating it to zero would make an
    # executor that runs nothing.
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 8)
    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: 0.5)
    monkeypatch.delenv(ner._WORKERS_ENV, raising=False)
    assert ner._pool_size() == 1


def test_the_deployment_may_say_so_itself(monkeypatch: pytest.MonkeyPatch) -> None:
    # None of the guesses can know that this process shares its CPUs with
    # something the kernel is not reporting.
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 64)
    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: None)
    monkeypatch.setenv(ner._WORKERS_ENV, "3")
    assert ner._pool_size() == 3

    monkeypatch.setenv(ner._WORKERS_ENV, "0")
    assert ner._pool_size() == 1, "a pool of zero is an executor that runs nothing"


def test_a_malformed_override_is_named_rather_than_obeyed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Guessing past a deployment's mistake silently is how it stays a mistake.
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 4)
    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: None)
    monkeypatch.setenv(ner._WORKERS_ENV, "two")
    with pytest.warns(RuntimeWarning, match="not an integer"):
        assert ner._pool_size() == 4


def test_one_text_never_claims_more_of_the_pool_than_exists() -> None:
    """The invariant, at every size rather than at this host's size.

    A floor of two survived review because it is wrong only when there is one
    CPU, and no test ran on one. Checking the function instead of the constant
    is what makes the small end reachable.
    """
    for pool in range(1, 65):
        flight = ner._in_flight(pool)
        assert 1 <= flight <= pool, f"{flight} in flight against {pool} workers"
        assert flight <= max(1, pool // 2), (
            f"one text may claim {flight} of {pool} workers, more than half — "
            "which is what lets a large document queue ahead of the next request"
        )


def test_the_shipped_constants_satisfy_it() -> None:
    # The function is what the tests above exercise; these are what `detect`
    # actually reads, and nothing else asserts they came from it.
    assert ner._in_flight(ner._POOL_SIZE) == ner._IN_FLIGHT
    assert ner._POOL_SIZE >= 1
