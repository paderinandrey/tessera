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

import pathlib

import pytest

import tessera_detector.ner as ner


def _write(directory, name: str, content: str) -> pathlib.Path:
    path = directory / name
    path.write_text(content)
    return path


def test_a_cgroup_v2_quota_is_read_as_cpus(tmp_path) -> None:
    (tmp_path / "cpu.max").write_text("200000 100000\n")
    assert ner._quota_at(tmp_path) == 2.0
    (tmp_path / "cpu.max").write_text("50000 100000\n")
    assert ner._quota_at(tmp_path) == 0.5, "half a CPU is a real limit and rounds later"
    # "max" is how cgroup v2 spells no limit, and reading it as a number is how
    # a limitless container would get a pool sized from a parse error.
    (tmp_path / "cpu.max").write_text("max 100000\n")
    assert ner._quota_at(tmp_path) is None


def test_a_cgroup_v1_quota_is_read_when_v2_is_absent(tmp_path) -> None:
    # A deployment does not choose which cgroup version its kernel exposes.
    (tmp_path / "cpu.cfs_quota_us").write_text("300000\n")
    (tmp_path / "cpu.cfs_period_us").write_text("100000\n")
    assert ner._quota_at(tmp_path) == 3.0
    # -1 is how cgroup v1 spells no limit.
    (tmp_path / "cpu.cfs_quota_us").write_text("-1\n")
    assert ner._quota_at(tmp_path) is None


def test_an_unreadable_cgroup_costs_the_default_and_not_an_exception(tmp_path) -> None:
    # This runs at import. A kernel that spells its files differently must cost
    # the default sizing, never a service that will not start.
    assert ner._quota_at(tmp_path / "absent") is None
    for content in ("", "nonsense", "200000", "abc def", "200000 0\n"):
        (tmp_path / "cpu.max").write_text(content)
        assert ner._quota_at(tmp_path) is None, f"{content!r} produced a number"


def _mountinfo(mountpoint: str, mount_root: str = "/", *, v2: bool = False) -> str:
    """One `/proc/self/mountinfo` line, in the kernel's own shape.

    The separator is a bare `-` with optional fields before it, which is why the
    parser finds it rather than counting columns.
    """
    if v2:
        return f"30 25 0:25 {mount_root} {mountpoint} rw,relatime shared:9 - cgroup2 cgroup2 rw\n"
    return (
        f"31 25 0:26 {mount_root} {mountpoint} rw,relatime shared:15 "
        "- cgroup cgroup rw,cpu,cpuacct\n"
    )


def test_the_quota_is_read_from_the_process_s_own_cgroup(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A container or a systemd unit is not at the hierarchy root.

    An early version read `/sys/fs/cgroup/cpu/cpu.cfs_quota_us` — the root,
    which is unlimited — while the process lived at `/system.slice/x.service`
    or `/docker/9f2c...`. So the quota came back `None` and the pool was sized
    from the affinity count again, one round after the quota itself was the
    finding.
    """
    mount = tmp_path / "cpu,cpuacct"
    mount.mkdir()
    monkeypatch.setattr(ner, "_PROC_SELF_MOUNTINFO", _write(tmp_path, "mountinfo",
                                                            _mountinfo(str(mount))))
    monkeypatch.setattr(ner, "_PROC_SELF_CGROUP", _write(tmp_path, "cgroup",
                                                         "12:cpu,cpuacct:/docker/abc123\n"))

    leaf = mount / "docker" / "abc123"
    leaf.mkdir(parents=True)
    (leaf / "cpu.cfs_quota_us").write_text("200000\n")
    (leaf / "cpu.cfs_period_us").write_text("100000\n")
    assert ner._cgroup_cpu_quota() == 2.0, "the root is unlimited; the leaf is not"


def test_a_combined_v1_mount_is_found(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    """`cpu,cpuacct` is one mount answering to two names.

    Guessing `/sys/fs/cgroup/cpu` missed it, and a guessed mountpoint was the
    fourth wrong answer to this question. The option list in `mountinfo` is
    where a mount says which controllers it carries.
    """
    mount = tmp_path / "cpu,cpuacct"
    (mount / "svc").mkdir(parents=True)
    monkeypatch.setattr(ner, "_PROC_SELF_MOUNTINFO", _write(tmp_path, "mountinfo",
                                                            _mountinfo(str(mount))))
    monkeypatch.setattr(
        ner, "_PROC_SELF_CGROUP", _write(tmp_path, "cgroup", "5:cpu,cpuacct:/svc\n")
    )
    (mount / "svc" / "cpu.cfs_quota_us").write_text("400000\n")
    (mount / "svc" / "cpu.cfs_period_us").write_text("100000\n")
    assert ner._cgroup_cpu_quota() == 4.0


def test_a_mount_showing_a_subtree_rebases_the_path(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A container often mounts its *own* cgroup as the hierarchy root, so
    # `/proc/self/cgroup` says `/docker/abc/inner` while on disk only `inner`
    # exists below the mountpoint. Joining the whole path would find nothing.
    mount = tmp_path / "cgroup"
    (mount / "inner").mkdir(parents=True)
    monkeypatch.setattr(
        ner, "_PROC_SELF_MOUNTINFO",
        _write(tmp_path, "mountinfo", _mountinfo(str(mount), "/docker/abc", v2=True)),
    )
    monkeypatch.setattr(ner, "_PROC_SELF_CGROUP", _write(tmp_path, "cgroup-of-self",
                                                         "0::/docker/abc/inner\n"))
    (mount / "inner" / "cpu.max").write_text("150000 100000\n")
    assert ner._cgroup_cpu_quota() == 1.5


def test_a_limit_on_an_ancestor_binds_too(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    # A service under a slice capped at two CPUs is capped at two whatever its
    # own directory says, so the narrowest of every level wins.
    mount = tmp_path / "cgroup"
    unit = mount / "system.slice" / "tessera.service"
    unit.mkdir(parents=True)
    monkeypatch.setattr(ner, "_PROC_SELF_MOUNTINFO", _write(tmp_path, "mountinfo",
                                                            _mountinfo(str(mount), v2=True)))
    monkeypatch.setattr(ner, "_PROC_SELF_CGROUP", _write(tmp_path, "cgroup-of-self",
                                                         "0::/system.slice/tessera.service\n"))
    (mount / "system.slice" / "cpu.max").write_text("200000 100000\n")
    (unit / "cpu.max").write_text("max 100000\n")
    assert ner._cgroup_cpu_quota() == 2.0, "the unit is unlimited; its slice is not"

    (unit / "cpu.max").write_text("100000 100000\n")
    assert ner._cgroup_cpu_quota() == 1.0, "and the tighter of the two wins"


def test_an_unreadable_proc_file_is_not_an_exception(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Not every kernel has these, and this runs at import.
    monkeypatch.setattr(ner, "_PROC_SELF_MOUNTINFO", tmp_path / "absent")
    monkeypatch.setattr(ner, "_PROC_SELF_CGROUP", tmp_path / "also-absent")
    assert ner._cgroup_cpu_quota() is None

    # A mountinfo this parser cannot read must cost the default, not a service
    # that will not start.
    for junk in ("", "nonsense", "31 25 0:26 / /mnt rw", "a - b"):
        monkeypatch.setattr(ner, "_PROC_SELF_MOUNTINFO", _write(tmp_path, "junk", junk))
        assert ner._cgroup_cpu_quota() is None, f"{junk!r} produced a quota"


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


def test_a_fractional_quota_rounds_up(monkeypatch: pytest.MonkeyPatch) -> None:
    # **Up, not down.** A worker consumes at most one CPU, so a container
    # entitled to 1.5 and given one worker can never use the half, and every
    # other request queues behind that worker for CPU time the cgroup was
    # willing to grant. Found in review of #63.
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 8)
    monkeypatch.delenv(ner._WORKERS_ENV, raising=False)

    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: 1.5)
    assert ner._pool_size() == 2

    # And `--cpus=0.5` is a real thing to write: truncating it to zero would
    # make an executor that runs nothing.
    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: 0.5)
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

    # Above the detected count, which is the whole point of an override: none of
    # the automatic answers can know this process has the machine to itself.
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 4)
    monkeypatch.setenv(ner._WORKERS_ENV, "8")
    assert ner._pool_size() == 8


def test_a_malformed_override_is_named_rather_than_obeyed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Guessing past a deployment's mistake silently is how it stays a mistake.
    monkeypatch.setattr(ner.os, "process_cpu_count", lambda: 4)
    monkeypatch.setattr(ner, "_cgroup_cpu_quota", lambda: None)
    monkeypatch.setenv(ner._WORKERS_ENV, "two")
    with pytest.warns(RuntimeWarning, match="not usable"):
        assert ner._pool_size() == 4

    # **Present and empty is malformed, not absent.** A Compose or Kubernetes
    # variable that expands to nothing is a deployment that tried to set a cap
    # and produced none — the case where falling back to a much larger automatic
    # pool in silence is worst. Found in review of #63.
    monkeypatch.setenv(ner._WORKERS_ENV, "")
    with pytest.warns(RuntimeWarning, match="not usable"):
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
