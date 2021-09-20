#!/bin/bash

set -ex

# test-logrotate
#
# Script that runs `examples/lines` on a file being written to and
# logrotate'd, to verify handling the rollover method of logrotate.
#

#logdir="$( mktemp -d )"
logdir="$1"
logfile="$logdir/foo.log"
outfile="$logdir/out.log"
rotatefile="$logdir/foo.conf"
statefile="$logdir/foo.state"

# essentially ubuntu's syslog config
cat >$rotatefile << EOL
$logfile
{
        nomissingok
        compress
        delaycompress
}
EOL

cat $rotatefile

#cargo build --example lines
#/target/debug/examples/lines --no-source $logfile > $outfile &
#inespid=$!

sleep 0.1

echo "foo" > $logfile
echo "bar" >> $logfile

sleep 0.1

touch $statefile
logrotate -vf -s $statefile $rotatefile

sleep 0.1

echo "baz" >> $logfile
echo "qux" >> $logfile

sleep 0.1

exit 0

#kill $linespid

out_check="$( cat $outfile )"
out_expect="$( cat << EOL
foo
bar
baz
qux
EOL
)"

ret=0
if [[ "$out_check" != "$out_expect" ]]; then
  ret=1
fi

rm $statefile
rm $rotatefile
rm $outfile
rm $logfile
rm $logfile.1 || echo 'rotated log no longer exists'
rmdir $logdir || ls $logdir

exit $ret
