//! The 38 journalled migrations, transcribed from
//! `packages/core/src/database/migration/*.ts`.
//!
//! # Why the incremental SQL has to exist at all
//!
//! [`crate::schema::up`] creates the *current* schema in one shot, which is all a
//! brand-new database needs. A database that predates the current schema needs
//! the intermediate steps, because the only truthful way to reach today's shape
//! from an older one is the sequence of edits that produced it — a `CREATE TABLE`
//! run over live history would destroy it. So this table is not a duplicate of
//! `schema.rs`; it is the other half of the same contract, and the two are
//! reconciled by measurement rather than by inspection: running every step here
//! against an empty database yields the same objects, columns, types, nullability,
//! defaults and primary keys as the user's real `opencode.db`, which the
//! TypeScript binary migrated itself.
//!
//! # Where the two shapes legitimately differ
//!
//! A migrated database is not column-for-column identical to a freshly created
//! one, and upstream has the same seam. `account_state.id` is `integer PRIMARY KEY
//! NOT NULL` here (migration 6) and `integer PRIMARY KEY` in `schema.gen.ts`;
//! `workspace.time_used` carries `DEFAULT 0` here (migration 18, where SQLite
//! demands a default to add a `NOT NULL` column) and none there. Both differences
//! are present in the real binary's own migrated database, so reproducing them is
//! fidelity, not drift.

use rusqlite::Transaction;

/// One journalled migration: the id recorded in the `migration` table, and the
/// edit it makes.
pub(crate) struct Migration {
    /// The id upstream writes into the journal. Also the migration file's name.
    pub(crate) id: &'static str,
    /// What the migration does.
    pub(crate) step: Step,
}

/// How a migration changes the database.
///
/// Almost every upstream migration is a fixed list of statements, so the default
/// representation is data rather than code — a table can be read against the
/// oracle line by line, and a `fn` per migration cannot.
pub(crate) enum Step {
    /// Statements run in order, each as its own batch.
    ///
    /// One element per upstream `tx.run(...)` call, so a migration that passes
    /// two statements in a single template literal stays a single element.
    Sql(&'static [&'static str]),
    /// `20260511173437_session-metadata`, which is conditional upstream.
    ///
    /// The column briefly shipped a second time under a migration that was later
    /// withdrawn, so upstream re-reads `pragma_table_info` and returns early when
    /// `session.metadata` is already present. Adding it unconditionally would fail
    /// with "duplicate column name" on exactly the installs that took that
    /// withdrawn release.
    AddSessionMetadataIfAbsent,
}

/// The migration chain in `migration.gen.ts` order.
///
/// Order is load-bearing twice over: it is the order the statements must run in,
/// and it is the order [`crate::migration::MIGRATION_IDS`] is derived from, which
/// a freshly created database seeds its journal with.
pub(crate) const MIGRATIONS: [Migration; 38] = [
    Migration {
        id: "20260127222353_familiar_lady_ursula",
        step: Step::Sql(&[
            "CREATE TABLE `project` (
               `id` text PRIMARY KEY,
               `worktree` text NOT NULL,
               `vcs` text,
               `name` text,
               `icon_url` text,
               `icon_color` text,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               `time_initialized` integer,
               `sandboxes` text NOT NULL
             );",
            "CREATE TABLE `message` (
               `id` text PRIMARY KEY,
               `session_id` text NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               `data` text NOT NULL,
               CONSTRAINT `fk_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
            "CREATE TABLE `part` (
               `id` text PRIMARY KEY,
               `message_id` text NOT NULL,
               `session_id` text NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               `data` text NOT NULL,
               CONSTRAINT `fk_part_message_id_message_id_fk` FOREIGN KEY (`message_id`) REFERENCES `message`(`id`) ON DELETE CASCADE
             );",
            "CREATE TABLE `permission` (
               `project_id` text PRIMARY KEY,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               `data` text NOT NULL,
               CONSTRAINT `fk_permission_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
             );",
            "CREATE TABLE `session` (
               `id` text PRIMARY KEY,
               `project_id` text NOT NULL,
               `parent_id` text,
               `slug` text NOT NULL,
               `directory` text NOT NULL,
               `title` text NOT NULL,
               `version` text NOT NULL,
               `share_url` text,
               `summary_additions` integer,
               `summary_deletions` integer,
               `summary_files` integer,
               `summary_diffs` text,
               `revert` text,
               `permission` text,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               `time_compacting` integer,
               `time_archived` integer,
               CONSTRAINT `fk_session_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
             );",
            "CREATE TABLE `todo` (
               `session_id` text NOT NULL,
               `content` text NOT NULL,
               `status` text NOT NULL,
               `priority` text NOT NULL,
               `position` integer NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               CONSTRAINT `todo_pk` PRIMARY KEY(`session_id`, `position`),
               CONSTRAINT `fk_todo_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
            "CREATE TABLE `session_share` (
               `session_id` text PRIMARY KEY,
               `id` text NOT NULL,
               `secret` text NOT NULL,
               `url` text NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               CONSTRAINT `fk_session_share_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
            "CREATE INDEX `message_session_idx` ON `message` (`session_id`);",
            "CREATE INDEX `part_message_idx` ON `part` (`message_id`);",
            "CREATE INDEX `part_session_idx` ON `part` (`session_id`);",
            "CREATE INDEX `session_project_idx` ON `session` (`project_id`);",
            "CREATE INDEX `session_parent_idx` ON `session` (`parent_id`);",
            "CREATE INDEX `todo_session_idx` ON `todo` (`session_id`);",
        ]),
    },
    Migration {
        id: "20260211171708_add_project_commands",
        step: Step::Sql(&["ALTER TABLE `project` ADD `commands` text;"]),
    },
    Migration {
        id: "20260213144116_wakeful_the_professor",
        step: Step::Sql(&[
            "CREATE TABLE `control_account` (
               `email` text NOT NULL,
               `url` text NOT NULL,
               `access_token` text NOT NULL,
               `refresh_token` text NOT NULL,
               `token_expiry` integer,
               `active` integer NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               CONSTRAINT `control_account_pk` PRIMARY KEY(`email`, `url`)
             );",
        ]),
    },
    Migration {
        id: "20260225215848_workspace",
        step: Step::Sql(&[
            "CREATE TABLE `workspace` (
               `id` text PRIMARY KEY,
               `branch` text,
               `project_id` text NOT NULL,
               `config` text NOT NULL,
               CONSTRAINT `fk_workspace_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
             );",
        ]),
    },
    Migration {
        id: "20260227213759_add_session_workspace_id",
        step: Step::Sql(&[
            "ALTER TABLE `session` ADD `workspace_id` text;",
            "CREATE INDEX `session_workspace_idx` ON `session` (`workspace_id`);",
        ]),
    },
    Migration {
        id: "20260228203230_blue_harpoon",
        step: Step::Sql(&[
            "CREATE TABLE `account` (
               `id` text PRIMARY KEY,
               `email` text NOT NULL,
               `url` text NOT NULL,
               `access_token` text NOT NULL,
               `refresh_token` text NOT NULL,
               `token_expiry` integer,
               `selected_org_id` text,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL
             );",
            "CREATE TABLE `account_state` (
               `id` integer PRIMARY KEY NOT NULL,
               `active_account_id` text,
               FOREIGN KEY (`active_account_id`) REFERENCES `account`(`id`) ON UPDATE no action ON DELETE set null
             );",
        ]),
    },
    Migration {
        id: "20260303231226_add_workspace_fields",
        step: Step::Sql(&[
            "ALTER TABLE `workspace` ADD `type` text NOT NULL;",
            "ALTER TABLE `workspace` ADD `name` text;",
            "ALTER TABLE `workspace` ADD `directory` text;",
            "ALTER TABLE `workspace` ADD `extra` text;",
            "ALTER TABLE `workspace` DROP COLUMN `config`;",
        ]),
    },
    Migration {
        id: "20260309230000_move_org_to_state",
        step: Step::Sql(&[
            "ALTER TABLE `account_state` ADD `active_org_id` text;",
            "UPDATE `account_state` SET `active_org_id` = (SELECT `selected_org_id` FROM `account` WHERE `account`.`id` = `account_state`.`active_account_id`);",
            "ALTER TABLE `account` DROP COLUMN `selected_org_id`;",
        ]),
    },
    Migration {
        id: "20260312043431_session_message_cursor",
        step: Step::Sql(&[
            "DROP INDEX IF EXISTS `message_session_idx`;",
            "DROP INDEX IF EXISTS `part_message_idx`;",
            "CREATE INDEX `message_session_time_created_id_idx` ON `message` (`session_id`,`time_created`,`id`);",
            "CREATE INDEX `part_message_id_id_idx` ON `part` (`message_id`,`id`);",
        ]),
    },
    Migration {
        id: "20260323234822_events",
        step: Step::Sql(&[
            "CREATE TABLE `event_sequence` (
               `aggregate_id` text PRIMARY KEY,
               `seq` integer NOT NULL
             );",
            "CREATE TABLE `event` (
               `id` text PRIMARY KEY,
               `aggregate_id` text NOT NULL,
               `seq` integer NOT NULL,
               `type` text NOT NULL,
               `data` text NOT NULL,
               CONSTRAINT `fk_event_aggregate_id_event_sequence_aggregate_id_fk` FOREIGN KEY (`aggregate_id`) REFERENCES `event_sequence`(`aggregate_id`) ON DELETE CASCADE
             );",
        ]),
    },
    Migration {
        id: "20260410174513_workspace-name",
        step: Step::Sql(&[
            "PRAGMA foreign_keys=OFF;",
            "CREATE TABLE `__new_workspace` (
               `id` text PRIMARY KEY,
               `type` text NOT NULL,
               `name` text DEFAULT '' NOT NULL,
               `branch` text,
               `directory` text,
               `extra` text,
               `project_id` text NOT NULL,
               CONSTRAINT `fk_workspace_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
             );",
            "INSERT INTO `__new_workspace`(`id`, `type`, `branch`, `name`, `directory`, `extra`, `project_id`) SELECT `id`, `type`, `branch`, `name`, `directory`, `extra`, `project_id` FROM `workspace`;",
            "DROP TABLE `workspace`;",
            "ALTER TABLE `__new_workspace` RENAME TO `workspace`;",
            "PRAGMA foreign_keys=ON;",
        ]),
    },
    Migration {
        id: "20260413175956_chief_energizer",
        step: Step::Sql(&[
            "CREATE TABLE `session_entry` (
               `id` text PRIMARY KEY,
               `session_id` text NOT NULL,
               `type` text NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               `data` text NOT NULL,
               CONSTRAINT `fk_session_entry_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
            "CREATE INDEX `session_entry_session_idx` ON `session_entry` (`session_id`);",
            "CREATE INDEX `session_entry_session_type_idx` ON `session_entry` (`session_id`,`type`);",
            "CREATE INDEX `session_entry_time_created_idx` ON `session_entry` (`time_created`);",
        ]),
    },
    Migration {
        id: "20260423070820_add_icon_url_override",
        // Upstream passes both statements to one `tx.run`, so they stay one
        // element and run as one batch. Whether Bun's driver executed the second
        // one is not observable on any database here — no project row has
        // `icon_url` set — so the literal text is the only available authority.
        step: Step::Sql(&[
            "ALTER TABLE `project` ADD `icon_url_override` text;
             UPDATE `project` SET `icon_url_override` = `icon_url` WHERE `icon_url` IS NOT NULL;",
        ]),
    },
    Migration {
        id: "20260427172553_slow_nightmare",
        step: Step::Sql(&[
            "CREATE TABLE `session_message` (
               `id` text PRIMARY KEY,
               `session_id` text NOT NULL,
               `type` text NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               `data` text NOT NULL,
               CONSTRAINT `fk_session_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
            "DROP INDEX IF EXISTS `session_entry_session_idx`;",
            "DROP INDEX IF EXISTS `session_entry_session_type_idx`;",
            "DROP INDEX IF EXISTS `session_entry_time_created_idx`;",
            "CREATE INDEX `session_message_session_idx` ON `session_message` (`session_id`);",
            "CREATE INDEX `session_message_session_type_idx` ON `session_message` (`session_id`,`type`);",
            "CREATE INDEX `session_message_time_created_idx` ON `session_message` (`time_created`);",
            "DROP TABLE `session_entry`;",
        ]),
    },
    Migration {
        id: "20260428004200_add_session_path",
        step: Step::Sql(&["ALTER TABLE `session` ADD `path` text;"]),
    },
    Migration {
        id: "20260501142318_next_venus",
        step: Step::Sql(&[
            "ALTER TABLE `session` ADD `agent` text;",
            "ALTER TABLE `session` ADD `model` text;",
        ]),
    },
    Migration {
        id: "20260504145000_add_sync_owner",
        step: Step::Sql(&["ALTER TABLE `event_sequence` ADD `owner_id` text;"]),
    },
    Migration {
        id: "20260507164347_add_workspace_time",
        step: Step::Sql(&[
            "ALTER TABLE `workspace` ADD `time_used` integer NOT NULL DEFAULT 0;",
        ]),
    },
    Migration {
        id: "20260510033149_session_usage",
        step: Step::Sql(&[
            "ALTER TABLE `session` ADD `cost` real DEFAULT 0 NOT NULL;",
            "ALTER TABLE `session` ADD `tokens_input` integer DEFAULT 0 NOT NULL;",
            "ALTER TABLE `session` ADD `tokens_output` integer DEFAULT 0 NOT NULL;",
            "ALTER TABLE `session` ADD `tokens_reasoning` integer DEFAULT 0 NOT NULL;",
            "ALTER TABLE `session` ADD `tokens_cache_read` integer DEFAULT 0 NOT NULL;",
            "ALTER TABLE `session` ADD `tokens_cache_write` integer DEFAULT 0 NOT NULL;",
            "UPDATE session
             SET
               cost = coalesce((
                 SELECT sum(coalesce(json_extract(message.data, '$.cost'), 0))
                 FROM message
                 WHERE message.session_id = session.id
                   AND json_extract(message.data, '$.role') = 'assistant'
               ), 0),
               tokens_input = coalesce((
                 SELECT sum(coalesce(json_extract(message.data, '$.tokens.input'), 0))
                 FROM message
                 WHERE message.session_id = session.id
                   AND json_extract(message.data, '$.role') = 'assistant'
               ), 0),
               tokens_output = coalesce((
                 SELECT sum(coalesce(json_extract(message.data, '$.tokens.output'), 0))
                 FROM message
                 WHERE message.session_id = session.id
                   AND json_extract(message.data, '$.role') = 'assistant'
               ), 0),
               tokens_reasoning = coalesce((
                 SELECT sum(coalesce(json_extract(message.data, '$.tokens.reasoning'), 0))
                 FROM message
                 WHERE message.session_id = session.id
                   AND json_extract(message.data, '$.role') = 'assistant'
               ), 0),
               tokens_cache_read = coalesce((
                 SELECT sum(coalesce(json_extract(message.data, '$.tokens.cache.read'), 0))
                 FROM message
                 WHERE message.session_id = session.id
                   AND json_extract(message.data, '$.role') = 'assistant'
               ), 0),
               tokens_cache_write = coalesce((
                 SELECT sum(coalesce(json_extract(message.data, '$.tokens.cache.write'), 0))
                 FROM message
                 WHERE message.session_id = session.id
                   AND json_extract(message.data, '$.role') = 'assistant'
               ), 0)",
        ]),
    },
    Migration {
        id: "20260511000411_data_migration_state",
        step: Step::Sql(&[
            "CREATE TABLE `data_migration` (
               `name` text PRIMARY KEY,
               `time_completed` integer NOT NULL
             );",
        ]),
    },
    Migration {
        id: "20260511173437_session-metadata",
        step: Step::AddSessionMetadataIfAbsent,
    },
    Migration {
        id: "20260601010001_normalize_storage_paths",
        step: Step::Sql(&[
            "UPDATE project SET worktree = REPLACE(worktree, char(92), '/') WHERE worktree GLOB '[A-Za-z]:' || char(92) || '*' OR worktree LIKE char(92) || char(92) || '%';",
            "UPDATE project SET sandboxes = REPLACE(sandboxes, char(92) || char(92), '/') WHERE instr(sandboxes, char(92)) > 0 AND (worktree GLOB '[A-Za-z]:*' OR worktree LIKE '//%');",
            "UPDATE session SET directory = REPLACE(directory, char(92), '/') WHERE directory GLOB '[A-Za-z]:' || char(92) || '*' OR directory LIKE char(92) || char(92) || '%';",
            "UPDATE session SET path = REPLACE(path, char(92), '/') WHERE path IS NOT NULL AND instr(path, char(92)) > 0 AND (directory GLOB '[A-Za-z]:*' OR directory LIKE '//%');",
        ]),
    },
    Migration {
        id: "20260601202201_amazing_prowler",
        step: Step::Sql(&["DROP TABLE `permission`;"]),
    },
    Migration {
        id: "20260602002951_lowly_union_jack",
        step: Step::Sql(&[
            "CREATE TABLE `permission` (
               `id` text PRIMARY KEY,
               `project_id` text NOT NULL,
               `action` text NOT NULL,
               `resource` text NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL,
               CONSTRAINT `fk_permission_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
             );",
            "CREATE UNIQUE INDEX `permission_project_action_resource_idx` ON `permission` (`project_id`,`action`,`resource`);",
        ]),
    },
    Migration {
        id: "20260602182828_add_project_directories",
        step: Step::Sql(&[
            "CREATE TABLE `project_directory` (
               `project_id` text NOT NULL,
               `directory` text NOT NULL,
               `type` text NOT NULL,
               `time_created` integer NOT NULL,
               CONSTRAINT `project_directory_pk` PRIMARY KEY(`project_id`, `directory`),
               CONSTRAINT `fk_project_directory_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
             );",
        ]),
    },
    Migration {
        id: "20260603001617_session_message_projection_indexes",
        step: Step::Sql(&[
            "DROP INDEX IF EXISTS `session_message_session_idx`;",
            "DROP INDEX IF EXISTS `session_message_session_type_idx`;",
            "CREATE INDEX `event_aggregate_seq_idx` ON `event` (`aggregate_id`,`seq`);",
            "CREATE INDEX `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);",
            "CREATE INDEX `session_message_session_type_time_created_id_idx` ON `session_message` (`session_id`,`type`,`time_created`,`id`);",
        ]),
    },
    Migration {
        id: "20260603040000_session_message_projection_order",
        // The `DELETE` is upstream's and is not incidental: pre-launch projections
        // were written before durable event persistence, so they cannot be given a
        // truthful `seq`. It also happens to be what makes the next statement
        // legal — SQLite refuses to add a `NOT NULL` column without a default to a
        // table that still has rows.
        step: Step::Sql(&[
            "DELETE FROM `session_message`;",
            "ALTER TABLE `session_message` ADD COLUMN `seq` integer NOT NULL;",
            "DROP INDEX IF EXISTS `session_message_session_type_time_created_id_idx`;",
            "CREATE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);",
            "CREATE INDEX `session_message_session_type_seq_idx` ON `session_message` (`session_id`,`type`,`seq`);",
        ]),
    },
    Migration {
        id: "20260603141458_session_input_inbox",
        step: Step::Sql(&[
            "CREATE TABLE `session_input` (
               `seq` integer PRIMARY KEY AUTOINCREMENT,
               `id` text NOT NULL UNIQUE,
               `session_id` text NOT NULL,
               `prompt` text NOT NULL,
               `delivery` text NOT NULL,
               `promoted_seq` integer,
               `time_created` integer NOT NULL,
               CONSTRAINT `fk_session_input_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
            "CREATE INDEX `session_input_session_pending_seq_idx` ON `session_input` (`session_id`,`promoted_seq`,`seq`);",
        ]),
    },
    Migration {
        id: "20260603160727_jittery_ezekiel_stane",
        step: Step::Sql(&[
            "DROP INDEX IF EXISTS `session_input_session_pending_seq_idx`;",
            "CREATE INDEX IF NOT EXISTS `event_aggregate_type_seq_idx` ON `event` (`aggregate_id`,`type`,`seq`);",
            "CREATE INDEX IF NOT EXISTS `session_input_session_pending_delivery_seq_idx` ON `session_input` (`session_id`,`promoted_seq`,`delivery`,`seq`);",
            "CREATE INDEX IF NOT EXISTS `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);",
        ]),
    },
    Migration {
        id: "20260604172448_event_sourced_session_input",
        step: Step::Sql(&[
            "DELETE FROM `session_input`;",
            "DELETE FROM `session_message`;",
            "DELETE FROM `event`;",
            "DELETE FROM `event_sequence`;",
            "UPDATE `session` SET `workspace_id` = NULL;",
            "DELETE FROM `workspace`;",
            "DROP INDEX IF EXISTS `event_aggregate_seq_idx`;",
            "CREATE UNIQUE INDEX `event_aggregate_seq_idx` ON `event` (`aggregate_id`,`seq`);",
            "DROP INDEX IF EXISTS `session_message_session_seq_idx`;",
            "CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);",
            "PRAGMA foreign_keys=OFF;",
            "CREATE TABLE `__new_session_input` (
               `id` text PRIMARY KEY,
               `session_id` text NOT NULL,
               `prompt` text NOT NULL,
               `delivery` text NOT NULL,
               `admitted_seq` integer NOT NULL,
               `promoted_seq` integer,
               `time_created` integer NOT NULL,
               CONSTRAINT `fk_session_input_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
            "DROP TABLE `session_input`;",
            "ALTER TABLE `__new_session_input` RENAME TO `session_input`;",
            "PRAGMA foreign_keys=ON;",
            "CREATE INDEX `session_input_session_pending_delivery_seq_idx` ON `session_input` (`session_id`,`promoted_seq`,`delivery`,`admitted_seq`);",
            "CREATE UNIQUE INDEX `session_input_session_admitted_seq_idx` ON `session_input` (`session_id`,`admitted_seq`);",
            "CREATE UNIQUE INDEX `session_input_session_promoted_seq_idx` ON `session_input` (`session_id`,`promoted_seq`);",
        ]),
    },
    Migration {
        id: "20260605003541_add_session_context_snapshot",
        step: Step::Sql(&[
            "CREATE TABLE `session_context_epoch` (
               `session_id` text PRIMARY KEY,
               `baseline` text NOT NULL,
               `snapshot` text NOT NULL,
               `baseline_seq` integer NOT NULL,
               `replacement_seq` integer,
               `revision` integer DEFAULT 0 NOT NULL,
               CONSTRAINT `fk_session_context_epoch_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
             );",
        ]),
    },
    Migration {
        id: "20260605042240_add_context_epoch_agent",
        step: Step::Sql(&[
            "ALTER TABLE `session_context_epoch` ADD `agent` text DEFAULT 'build' NOT NULL;",
        ]),
    },
    Migration {
        id: "20260611035744_credential",
        step: Step::Sql(&[
            "CREATE TABLE `credential` (
               `id` text PRIMARY KEY,
               `connector_id` text NOT NULL,
               `method_id` text NOT NULL,
               `label` text NOT NULL,
               `value` text NOT NULL,
               `active` integer DEFAULT false NOT NULL,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL
             );",
            "CREATE UNIQUE INDEX `credential_connector_active_idx` ON `credential` (`connector_id`) WHERE \"credential\".\"active\" = 1;",
        ]),
    },
    Migration {
        id: "20260611192811_lush_chimera",
        step: Step::Sql(&[
            "DROP INDEX IF EXISTS `credential_connector_active_idx`;",
            "DROP TABLE `credential`;",
            "CREATE TABLE `credential` (
               `id` text PRIMARY KEY,
               `integration_id` text,
               `label` text NOT NULL,
               `value` text NOT NULL,
               `connector_id` text,
               `method_id` text,
               `active` integer,
               `time_created` integer NOT NULL,
               `time_updated` integer NOT NULL
             );",
        ]),
    },
    Migration {
        id: "20260612174303_project_dir_strategy",
        step: Step::Sql(&[
            "ALTER TABLE `project_directory` ADD `strategy` text;",
            "PRAGMA foreign_keys=OFF;",
            "CREATE TABLE `__new_project_directory` (
               `project_id` text NOT NULL,
               `directory` text NOT NULL,
               `type` text,
               `strategy` text,
               `time_created` integer NOT NULL,
               CONSTRAINT `project_directory_pk` PRIMARY KEY(`project_id`, `directory`),
               CONSTRAINT `fk_project_directory_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
             );",
            "INSERT INTO `__new_project_directory`(`project_id`, `directory`, `type`, `time_created`) SELECT `project_id`, `directory`, `type`, `time_created` FROM `project_directory`;",
            "DROP TABLE `project_directory`;",
            "ALTER TABLE `__new_project_directory` RENAME TO `project_directory`;",
            "PRAGMA foreign_keys=ON;",
        ]),
    },
    Migration {
        id: "20260622142730_simplify_session_context_epoch",
        step: Step::Sql(&[
            "ALTER TABLE `session_context_epoch` DROP COLUMN `agent`;",
            "ALTER TABLE `session_context_epoch` DROP COLUMN `replacement_seq`;",
            "ALTER TABLE `session_context_epoch` DROP COLUMN `revision`;",
        ]),
    },
    Migration {
        id: "20260622170816_reset_v2_session_state",
        step: Step::Sql(&[
            "DELETE FROM `session_context_epoch`;",
            "DELETE FROM `session_input`;",
            "DELETE FROM `session_message`;",
            "DELETE FROM `event`;",
            "DELETE FROM `event_sequence`;",
            "UPDATE `session` SET `workspace_id` = NULL WHERE `workspace_id` IS NOT NULL;",
            "DELETE FROM `workspace`;",
        ]),
    },
    Migration {
        id: "20260622202450_simplify_session_input",
        step: Step::Sql(&[
            "DELETE FROM `session_context_epoch`;",
            "DELETE FROM `session_input`;",
            "DELETE FROM `session_message`;",
            "DELETE FROM `event`;",
            "DELETE FROM `event_sequence`;",
            "UPDATE `session` SET `workspace_id` = NULL WHERE `workspace_id` IS NOT NULL;",
            "DELETE FROM `workspace`;",
        ]),
    },
];

impl Migration {
    /// Run this migration's statements against `transaction`.
    ///
    /// # Errors
    ///
    /// [`oc_error::DbError::Migration`] naming the failing statement.
    pub(crate) fn run(&self, transaction: &Transaction<'_>) -> Result<(), oc_error::DbError> {
        match self.step {
            Step::Sql(statements) => {
                for statement in statements {
                    transaction
                        .execute_batch(statement)
                        .map_err(super::map_error)?;
                }
                Ok(())
            }
            Step::AddSessionMetadataIfAbsent => {
                if session_has_metadata(transaction)? {
                    return Ok(());
                }
                transaction
                    .execute_batch("ALTER TABLE `session` ADD `metadata` text;")
                    .map_err(super::map_error)
            }
        }
    }
}

fn session_has_metadata(transaction: &Transaction<'_>) -> Result<bool, oc_error::DbError> {
    transaction
        .query_row(
            "SELECT count(*) FROM pragma_table_info('session') WHERE name = 'metadata'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .map_err(super::map_error)
}
