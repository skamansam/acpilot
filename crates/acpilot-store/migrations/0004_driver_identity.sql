-- Who asked.
--
-- A session opened through the ACP bridge belongs to the driver that opened it,
-- and the permission decisions taken during its turns are that driver's audit
-- trail. NULL means a person at the machine, or a row from before the bridge
-- recorded drivers; both read the same way, and neither invents an identity.
ALTER TABLE sessions ADD COLUMN driver_id TEXT;
ALTER TABLE grants ADD COLUMN driver_id TEXT;
