"""Physical-request admission is durable even when transport or accounting fails."""

import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal, localcontext
from pathlib import Path

import pytest
from pydantic import ValidationError

from fastbrowse_evals.spend import ChargeState, RequestWallet, SpendStop


def wallet(path: Path, limit: str = "1") -> RequestWallet:
    return RequestWallet(path, campaign="fixture-campaign", limit=Decimal(limit))


def test_admission_survives_restart_and_unknown_reconciliation(tmp_path: Path):
    path = tmp_path / "wallet.jsonl"
    with wallet(path) as spend:
        spend.reserve("first", quote_id="fixture-price", bound=Decimal("0.7"))
        spend.settle("first", None)
        assert spend.committed == Decimal("0.7")
    with wallet(path) as spend:
        assert spend.charges[0].state == ChargeState.UNKNOWN
        with pytest.raises(SpendStop, match="approved total"):
            spend.reserve("second", quote_id="fixture-price", bound=Decimal("0.4"))
        spend.settle("first", Decimal("0.1"))
        spend.reserve("second", quote_id="fixture-price", bound=Decimal("0.9"))
        assert spend.committed == 1
        with pytest.raises(ValueError, match="reserve twice"):
            spend.reserve("first", quote_id="fixture-price", bound=Decimal("0.1"))
        with pytest.raises(ValueError, match="unresolved"):
            spend.settle("first", Decimal("0"))
    with wallet(path) as spend:
        assert spend.committed == 1
        assert spend.charges[1].state == ChargeState.RESERVED


def test_actual_overrun_is_preserved_and_blocks_further_dispatch(tmp_path: Path):
    path = tmp_path / "wallet.jsonl"
    with wallet(path, "10") as spend:
        spend.reserve("wrong-quote", quote_id="fixture-price", bound=Decimal("1"))
        with pytest.raises(SpendStop, match="actual charge retained"):
            spend.settle("wrong-quote", Decimal("2"))
        assert spend.committed == 2
        with pytest.raises(SpendStop, match="exceeded"):
            spend.reserve("next", quote_id="fixture-price", bound=Decimal("1"))
    with wallet(path, "10") as spend:
        assert spend.committed == 2
        with pytest.raises(SpendStop, match="exceeded"):
            spend.reserve("after-restart", quote_id="fixture-price", bound=Decimal("1"))


def test_threads_cannot_oversubscribe_or_duplicate_an_attempt(tmp_path: Path):
    with wallet(tmp_path / "wallet.jsonl") as spend, ThreadPoolExecutor(max_workers=8) as pool:

        def reserve(index: int):
            try:
                spend.reserve(str(index), quote_id="fixture-price", bound=Decimal("0.1"))
                return True
            except SpendStop:
                return False

        assert sum(pool.map(reserve, range(40))) == 10
        assert spend.committed == 1
    with wallet(tmp_path / "duplicates.jsonl") as spend, ThreadPoolExecutor(max_workers=8) as pool:

        def duplicate(_: int):
            try:
                spend.reserve("same", quote_id="fixture-price", bound=Decimal("0.1"))
                return True
            except ValueError:
                return False

        assert sum(pool.map(duplicate, range(20))) == 1


def test_exact_amounts_do_not_depend_on_decimal_context(tmp_path: Path):
    with localcontext() as context:
        context.prec = 2
        with wallet(tmp_path / "wallet.jsonl") as spend:
            spend.reserve("a", quote_id="fixture-price", bound=Decimal("0.123456789012345678"))
            spend.reserve("b", quote_id="fixture-price", bound=Decimal("0.000000000000000001"))
            spend.settle("b", Decimal("-0.0000000000000000000"))
            assert spend.committed == Decimal("0.123456789012345678")


@pytest.mark.parametrize("amount", ["NaN", "Infinity", "-1", "0.0000000000000000001"])
def test_invalid_amounts_never_reach_journal(tmp_path: Path, amount: str):
    with wallet(tmp_path / "wallet.jsonl") as spend:
        with pytest.raises(ValidationError):
            spend.reserve("bad", quote_id="fixture-price", bound=Decimal(amount))
        assert not spend.charges


def test_lock_and_identity_are_required_on_resume(tmp_path: Path):
    path = tmp_path / "wallet.jsonl"
    with wallet(path), pytest.raises(SpendStop, match="another process"):
        wallet(path)
    with pytest.raises(ValueError, match="original campaign"):
        wallet(path, "2")
    with pytest.raises(ValueError, match="original campaign"):
        RequestWallet(path, campaign="different", limit=Decimal("1"))
    with wallet(path):
        pass


@pytest.mark.parametrize("suffix", [b'{"kind":', b"{}\n", b"\n"])
def test_corrupt_journal_never_drops_a_reservation(tmp_path: Path, suffix: bytes):
    path = tmp_path / "wallet.jsonl"
    with wallet(path) as spend:
        spend.reserve("in-flight", quote_id="fixture-price", bound=Decimal("1"))
    with path.open("ab") as output:
        output.write(suffix)
    with pytest.raises(ValueError):
        wallet(path)


def test_failed_fsync_poisoning_prevents_send_authorization(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    path = tmp_path / "wallet.jsonl"
    with wallet(path) as spend:
        with monkeypatch.context() as patch:

            def failed(_: int):
                raise OSError("fixture disk failure")

            patch.setattr(os, "fsync", failed)
            with pytest.raises(OSError, match="disk failure"):
                spend.reserve("never-sent", quote_id="fixture-price", bound=Decimal("1"))
        with pytest.raises(SpendStop, match="journal failure"):
            spend.reserve("next", quote_id="fixture-price", bound=Decimal("1"))
    with wallet(path) as spend:
        assert spend.committed == 1


def test_short_writes_finish_before_reservation_returns(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    write = os.write
    with monkeypatch.context() as patch:
        patch.setattr(os, "write", lambda fd, data: write(fd, data[:3]))
        with wallet(tmp_path / "wallet.jsonl") as spend:
            spend.reserve("first", quote_id="fixture-price", bound=Decimal("1"))
    with wallet(tmp_path / "wallet.jsonl") as spend:
        assert spend.committed == 1


def test_a_process_exit_keeps_the_sent_request_reserved(tmp_path: Path):
    path = tmp_path / "wallet.jsonl"
    script = """
import os, sys
from decimal import Decimal
from pathlib import Path
from fastbrowse_evals.spend import RequestWallet
wallet = RequestWallet(Path(sys.argv[1]), campaign="fixture-campaign", limit=Decimal("1"))
wallet.reserve("interrupted", quote_id="fixture-price", bound=Decimal("1"))
os._exit(0)
"""
    subprocess.run([sys.executable, "-c", script, str(path)], check=True)
    with wallet(path) as spend:
        assert spend.committed == 1
        with pytest.raises(SpendStop, match="approved total"):
            spend.reserve("next", quote_id="fixture-price", bound=Decimal("0.01"))


def test_another_process_cannot_own_the_same_journal(tmp_path: Path):
    path = tmp_path / "wallet.jsonl"
    script = """
import sys
from decimal import Decimal
from pathlib import Path
from fastbrowse_evals.spend import RequestWallet, SpendStop
try:
    RequestWallet(Path(sys.argv[1]), campaign="fixture-campaign", limit=Decimal("1"))
except SpendStop:
    sys.exit(0)
sys.exit(1)
"""
    with wallet(path):
        subprocess.run([sys.executable, "-c", script, str(path)], check=True)
