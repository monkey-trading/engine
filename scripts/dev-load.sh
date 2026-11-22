#!/usr/bin/env bash
# The node load dev scenario: the launch book of 100k accounts on the production genesis's 34 markets
# taken over by a node in one process, its feeds reading loopback stand-ins for every connection the
# production template opens, driven through its TLS API and WebSocket stream by a load generator in a
# second process: signed market and limit orders, cancels, cancel-alls and close-alls from 2,000
# traders under session keys, through the API's trusted PROXY protocol peer, and every stream
# subscriber the template allows, half signed. It runs launch flow, then ten times it, two minutes
# each, and with --ramp a ramp from twenty times launch flow, doubling each minute, until every budget
# has broken or the sequencer is saturated, then prints the load at which each budget first broke.
# Each leg prints order-to-fill and price-to-stamp at p50, p99 and p999, the sequencer's
# batch time, stream lag, the sequencer's busy share, and the process's CPU and resident memory,
# against the launch bar, and, as each window does, how long the host's tasks waited on storage and for a
# CPU where the host reports it; the run's log then replays to its roots. It fails if either measured leg
# breaks the bar, with a loaded runner's tolerance, or did not measure what it claims: a request
# refused, rejected or unanswered, a subscriber that took no frame, lost its connection or had not
# taken the leg's last commands, or no fill or stamped price. The 5 ms order-to-fill p50 bar is held on
# CI's Linux runners; on macOS each batch's sync is a full flush of about 3.5 ms, which puts a Mac's p50
# near the bar.
#
# Needs only Cargo: the venues and the screening source are loopback stand-ins, on free ports, with
# private temporary data. The screening source answers each call after a remote source's round trip,
# LOAD_SOURCE_DELAY_MS (200); the bar holds the market orders of traders that sent a screened request
# within the screening bound, whose decisions the node keeps fresh, and those of traders that wait for
# the source, or may, or that queue behind another trader's wait on their client's one connection,
# print apart. The environment sizes the run:
# LOAD_ACCOUNTS (100000), LOAD_TRADERS (2000),
# LOAD_SUBSCRIBERS (256), LOAD_LEG_SECS (120), LOAD_CRYPTO_RATE (every crypto market's price commands
# a second; by default each market's median second in `dev-venue-load.sh --soak`, whose busiest
# second, 116 for BTC-USD, a run at LOAD_CRYPTO_RATE=116 holds throughout) and LOAD_RWA_MOVES (each
# other market's mid moves a second on each of its connections, at most two on CME Group's). CI runs
# it at 10,000 accounts.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
if [[ "${1:-}" == "--ramp" ]]; then
  export LOAD_RAMP=1
fi
test=load_scenario::dev_scenario_load
cargo_test=(cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib "$test" -- --ignored --exact)
# A renamed or moved test would otherwise pass by running nothing. The whole list is read before it is
# matched: `grep -q` closing the pipe at its first match fails the list's writer under pipefail.
listed="$(CARGO_INCREMENTAL=0 "${cargo_test[@]}" --list)"
grep -qx "$test: test" <<<"$listed" || {
  echo "the dev scenario test $test was not found" >&2
  exit 1
}
# The build leaves its output, some 460 MB on CI's runners, in the page cache, and the kernel writes it
# back once it is 30 seconds old (vm.dirty_expire_centisecs): on CI's book, inside the first leg, where
# that burst held the log's syncs, and every order waiting on one, for up to 250 ms. Writing it back
# first leaves the legs to the node's own writes.
sync
# Experiment for #1180, not for merging: the host's facts, then the scenario three times on this runner,
# on the disk, on tmpfs and on the disk again, with a per-second trace of the host's storage beside it.
exp="$(mktemp -d)"
cat > "$exp/trace.pl" <<'TRACE'
#!/usr/bin/perl
# Experiment for #1180, not for merging: prints one EXPTRACE line a second with the host's dirty and
# writeback memory, its io and cpu pressure totals, the pages dirtied and written, the cpu line of
# /proc/stat, every block device's counters, and the processes that wrote most in that second.
use strict;
use warnings;
use Time::HiRes qw(time sleep);

$| = 1;
my %prev;
sub slurp {
    my ($path) = @_;
    open my $f, '<', $path or return '';
    local $/;
    my $text = <$f>;
    close $f;
    return $text // '';
}
while (1) {
    my $t = time;
    my $mem = slurp('/proc/meminfo');
    my ($dirty) = $mem =~ /^Dirty:\s+(\d+)/m;
    my ($wb) = $mem =~ /^Writeback:\s+(\d+)/m;
    my @io = slurp('/proc/pressure/io') =~ /total=(\d+)/g;
    my @cpu = slurp('/proc/pressure/cpu') =~ /total=(\d+)/g;
    my $vm = slurp('/proc/vmstat');
    my ($dirtied) = $vm =~ /^nr_dirtied (\d+)/m;
    my ($written) = $vm =~ /^nr_written (\d+)/m;
    my ($stat) = slurp('/proc/stat') =~ /^(cpu .*)$/m;
    my @disks;
    for my $line (split /\n/, slurp('/proc/diskstats')) {
        my @x = split ' ', $line;
        next if $x[2] =~ /^(loop|ram|zram)/;
        push @disks, join ' ', @x[2, 3, 5, 7, 9, 10, 11, 12, 13], $x[18] // 0, $x[19] // 0;
    }
    my (%now, @top);
    for my $dir (glob '/proc/[0-9]*') {
        my ($pid) = $dir =~ /(\d+)$/;
        my ($bytes) = slurp("$dir/io") =~ /^write_bytes: (\d+)/m;
        next unless defined $bytes;
        $now{$pid} = $bytes;
        my $delta = $bytes - ($prev{$pid} // $bytes);
        next unless $delta > 0;
        my $comm = slurp("$dir/comm");
        chomp $comm;
        $comm =~ s/\s+/_/g;
        push @top, [$delta, "$pid:$comm"];
    }
    %prev = %now;
    my @writers = map { "$_->[1]:$_->[0]" } (sort { $b->[0] <=> $a->[0] } @top)[0 .. ($#top < 5 ? $#top : 5)];
    printf "EXPTRACE %.3f dirty=%s wb=%s io=%s cpu=%s vm=%s,%s | %s | %s | %s\n", $t, $dirty // '-', $wb // '-',
        join(',', @io), join(',', @cpu), $dirtied // '-', $written // '-', $stat // '-', join('; ', @disks),
        join(' ', @writers);
    my $left = 1 - (time - $t);
    sleep $left if $left > 0;
}
TRACE
{
  uname -a; nproc; free -m; lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT,ROTA,MODEL; findmnt -T /tmp; findmnt -T /dev/shm
  findmnt -T /mnt || true; df -h / /tmp /mnt /dev/shm || true; swapon --show || true
  for q in /sys/block/*/queue; do echo "$q $(cat $q/scheduler) rot=$(cat $q/rotational) nr=$(cat $q/nr_requests) wc=$(cat $q/write_cache 2>/dev/null)"; done
  sysctl vm.dirty_ratio vm.dirty_background_ratio vm.dirty_expire_centisecs vm.dirty_writeback_centisecs vm.dirty_bytes vm.dirty_background_bytes
  grep -m1 'model name' /proc/cpuinfo; cat /sys/class/dmi/id/product_name /sys/class/dmi/id/sys_vendor 2>/dev/null || true
  systemctl list-units --type=service --state=running --no-pager --no-legend || true
  systemctl list-timers --all --no-pager || true
  ps -eo pid,ppid,user,ni,stat,pcpu,rss,comm --sort=-pcpu | head -40
} 2>&1 | sed 's/^/EXPHOST /'
sudo perl "$exp/trace.pl" &
tracer=$!
trap 'sudo kill $tracer 2>/dev/null || true' EXIT
for at in a_disk:/tmp b_tmpfs:/dev/shm c_disk:/tmp; do
  name="${at%%:*}"
  dir="$(mktemp -d -p "${at#*:}")"
  echo "EXPRUN $name start $(date +%s.%N) TMPDIR=$dir"
  if TMPDIR="$dir" CARGO_INCREMENTAL=0 "${cargo_test[@]}" --nocapture; then
    echo "EXPRUN $name passed $(date +%s.%N)"
  else
    echo "EXPRUN $name failed $(date +%s.%N)"
  fi
  rm -rf "$dir"
done
ps -eo pid,ppid,user,ni,stat,pcpu,rss,comm --sort=-pcpu | head -20 | sed 's/^/EXPHOST end /'
