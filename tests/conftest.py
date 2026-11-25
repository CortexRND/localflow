import pytest


@pytest.fixture(autouse=True)
def _isolated_perf_ledger(tmp_path_factory, monkeypatch):
    """Code under test records perf events by default (`perf_metrics = true`);
    keep them out of the developer's real ~/.localflow/perf."""
    path = tmp_path_factory.mktemp("perf-ledger") / "events.jsonl"
    monkeypatch.setenv("LOCALFLOW_PERF_LEDGER", str(path))
