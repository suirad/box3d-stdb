#!/usr/bin/env fish

set DB box3d-memtest

spacetime publish -p examples/test-module $DB --delete-data=always --yes
or begin; echo "FAIL: publish failed"; exit 1; end

# awk NR==3 assumes ASCII-table header+sep+data; adjust if CLI output format changes
function _sql_val
    spacetime sql $DB $argv[1] | awk 'NR==3 {match($0, /[0-9]+/, a); print a[0]}'
end

function probe
    spacetime call $DB probe_mem "\"$argv[1]\""
end

function get_bytes
    _sql_val "select byte_count from mem_probe where label = '$argv[1]'"
end

function get_worlds
    _sql_val "select world_count from mem_probe where label = '$argv[1]'"
end

set pass 0
set fail 0

function assert_eq
    if test "$argv[2]" = "$argv[3]"
        echo "PASS: $argv[1] ($argv[2])"
        set -g pass (math $pass + 1)
    else
        echo "FAIL: $argv[1] — got $argv[2], expected $argv[3]"
        set -g fail (math $fail + 1)
    end
end

probe baseline
set b0 (get_bytes baseline)
set w0 (get_worlds baseline)

spacetime call $DB step_world 100
probe after-create
set b1 (get_bytes after-create)
set w1 (get_worlds after-create)
assert_eq "worlds after create" $w1 (math $w0 + 1)
if test (math $b1 - $b0) -gt 0
    echo "PASS: bytes increased after create ($b1 > $b0)"
    set -g pass (math $pass + 1)
else
    echo "FAIL: bytes should increase after create ($b1 <= $b0)"
    set -g fail (math $fail + 1)
end

spacetime call $DB remove_world 100
probe after-destroy
assert_eq "bytes after destroy" (get_bytes after-destroy) $b0
assert_eq "worlds after destroy" (get_worlds after-destroy) $w0

# reducer failure is expected; don't abort
spacetime call $DB fail_rebuild 101; or true
probe after-failed-rebuild
assert_eq "bytes after failed rebuild" (get_bytes after-failed-rebuild) $b0
assert_eq "worlds after failed rebuild" (get_worlds after-failed-rebuild) $w0

spacetime call $DB step_world 200
probe before-poison
set B1 (get_bytes before-poison)
set W1 (get_worlds before-poison)

spacetime call $DB poison_world 200
probe after-poison
assert_eq "bytes after poison (world still resident)" (get_bytes after-poison) $B1

spacetime call $DB step_world 200
probe after-repoison-rebuild
assert_eq "worlds after repoison rebuild (old leaked + new)" (get_worlds after-repoison-rebuild) (math $W1 + 1)

spacetime call $DB remove_world 200
probe final
set bf (get_bytes final)
set wf (get_worlds final)
assert_eq "worlds final (leaked slot persists)" $wf $W1
assert_eq "bytes final == B1 (leaked C world)" $bf $B1
echo "leaked bytes vs baseline: "(math $bf - $b0)" (bounded I6 leak — one empty world's allocations)"

echo ""
echo "Results: $pass passed, $fail failed"
test $fail -eq 0
