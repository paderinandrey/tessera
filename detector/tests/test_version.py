"""The version is what the gateway keys its span cache by, so it must change
whenever the same text would produce different spans."""

from tessera_detector.version import detector_version, version_from


def test_the_same_inputs_give_the_same_version():
    first = version_from("gliner@abc123", [b"identifiers", b"ner"])
    second = version_from("gliner@abc123", [b"identifiers", b"ner"])
    assert first == second


def test_changing_the_weights_changes_the_version():
    pinned = version_from("gliner@abc123", [b"identifiers", b"ner"])
    bumped = version_from("gliner@def456", [b"identifiers", b"ner"])
    assert pinned != bumped


def test_editing_either_catalog_changes_the_version():
    # A threshold edit changes what is detected without touching HF_REVISION.
    # If the version missed that, the gateway would serve spans from the old
    # thresholds until its cache aged out.
    base = version_from("gliner@abc123", [b"identifiers", b"ner"])
    first_edited = version_from("gliner@abc123", [b"identifiers!", b"ner"])
    second_edited = version_from("gliner@abc123", [b"identifiers", b"ner!"])
    assert base != first_edited
    assert base != second_edited
    assert first_edited != second_edited


def test_catalog_order_is_not_a_concatenation_accident():
    # Hashing each catalog before folding it in means a byte moving across the
    # boundary between two catalogs cannot leave the version unchanged.
    assert version_from("m", [b"ab", b"c"]) != version_from("m", [b"a", b"bc"])


def test_the_real_detector_reports_a_stable_version():
    assert detector_version() == detector_version()
    assert len(detector_version()) == 32
