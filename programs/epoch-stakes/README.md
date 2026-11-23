This is the runtime prototype for SIMD-0676. The account layout follows the
264-byte entry format in the November 19 draft, with eight retained epochs.
Version 1 has not been released or activated.

Publication copies the admitted boundary vote states before reward distribution.
Snapshot loading validates the restored epoch accounts against the historical
manifest and rebuilds their cache fields. Full historical vote states remain in
the manifest for authorized voters, migration rewards and other omitted inputs.
The cache retains eight epochs while this feature is active so those legacy
inputs remain available for every retained account. No snapshot wire fields are
removed, and collector payment rules are unchanged.

This revision supports vote-state formats through V4. Those formats have scalar
commissions and no registered location, so the location field is zero. The
format can encode the signed coordinates specified by SIMD-0674. Reading future
location fields and selecting future V5 commission schedules requires the
corresponding vote-state APIs to land first.

Tests cover the wire offsets and absent values, invalid accounts, publication
and closure, funding, raw commissions, old reward tuples, excluded BLS entries,
and full and incremental snapshot archive restoration. The feature remains
unactivated and this branch is a prototype, not a release candidate.
