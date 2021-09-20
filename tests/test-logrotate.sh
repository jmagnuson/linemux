#!/bin/bash

set -ex

# test-logrotate
#
# Writes some lines to a file, forcing a logrotate in between writes to
# verify linemux correctly tracks the rotation.

logdir="$1"
logfile="$logdir/foo.log"
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
