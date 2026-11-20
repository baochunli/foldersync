# State and recovery

The app will own private local mapping and run records. Each mapping
generation will keep its own rclone work directory. Preserve that directory
after an interrupted run and keep recovery copies separate from live sync
trees.

After a crash or disconnect, revalidate endpoints and volume identity before
continuing. A missing or damaged work directory, or a change to a mapping's
identity, requires an explicit recovery decision. Initialization and resync
must be deliberate actions.

**TODO** verify cancellation and restart behavior, recovery copies, and resync
procedures using disposable fixtures.
