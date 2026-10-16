"""Interactive CLI for deleting and archiving your own messages.

Delete YOUR messages from DMs or servers: the channel is scanned
newest-to-oldest one page at a time and each of your messages in the chosen
range is deleted as it is found, with live progress and a resumable
per-channel session. Uses the platform's unofficial user API (self-bot).
WARNING: Self-bots violate the platform ToS. Use at your own risk.

Entry point: guillotine.py"""


import argparse
import re
import threading
import webbrowser
from datetime import datetime, timedelta

from message_guillotine.api import RATE_LIMIT_DELAY, MessageGuillotine
from message_guillotine.archive_format import (
    find_existing_archive,
    list_archives,
    read_meta,
)
from message_guillotine.archiver import archive_chat
from message_guillotine.config import configure_archive_dir, resolve_archive_dir
from message_guillotine.deletion import load_session, run_deletion
from message_guillotine.viewer.server import create_server

FILTER_THRESHOLD = 12  # lists longer than this offer a name filter


def _relative_datetime(text):
    """A relative date as a datetime at local midnight, or None.

    Accepted: 'today', 'yesterday', '7d', '2w', '3d ago'."""
    text = text.strip().lower()
    if text == "today":
        return datetime.now().replace(hour=0, minute=0, second=0, microsecond=0)
    if text == "yesterday":
        return (datetime.now() - timedelta(days=1)).replace(
            hour=0, minute=0, second=0, microsecond=0)
    match = re.fullmatch(r"(\d+)\s*([dw])\s*(?:ago)?", text)
    if match:
        days = int(match.group(1)) * (7 if match.group(2) == "w" else 1)
        return (datetime.now() - timedelta(days=days)).replace(
            hour=0, minute=0, second=0, microsecond=0)
    return None


def parse_datetime(prompt):
    """Parse user input into datetime. Blank means no limit."""
    while True:
        date_str = input(prompt).strip()
        if not date_str:
            return None

        relative = _relative_datetime(date_str)
        if relative is not None:
            return relative

        try:
            for fmt in ["%Y-%m-%d", "%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S", "%m/%d/%Y", "%m/%d/%Y %H:%M"]:
                try:
                    return datetime.strptime(date_str, fmt)
                except ValueError:
                    continue
            return datetime.fromisoformat(date_str.replace('Z', '+00:00'))
        except ValueError:
            print(f"Invalid format. Use: YYYY-MM-DD, YYYY-MM-DD HH:MM:SS, or 7d/today")


def _choose_numbered(items, describe, prompt, title):
    """Print a numbered list and prompt until a valid selection. Lists longer
    than FILTER_THRESHOLD first offer a name filter. Returns the chosen item,
    or None on cancel or empty filter result."""
    print("\n" + "="*80)
    print(title)
    print("="*80)

    chosen = items
    if len(items) > FILTER_THRESHOLD:
        while True:
            query = input("Filter by name (blank to list all): ").strip().lower()
            if not query:
                chosen = items
                break
            chosen = [item for item in items if query in describe(item).lower()]
            if chosen:
                break
            print("Nothing matches that filter; try again or leave blank.")

    for i, item in enumerate(chosen, 1):
        print(f"[{i}] {describe(item)}")

    print("\n[0] Cancel")

    while True:
        raw = input(prompt).strip()
        try:
            choice = int(raw)
        except ValueError:
            print("Invalid selection.")
            continue
        if choice == 0:
            return None
        if 1 <= choice <= len(chosen):
            return chosen[choice - 1]
        print("Invalid selection.")


def select_dm_user(tool):
    """Fetch DM channels and prompt the user to select one. Returns the user dict or None."""
    if not tool.get_dm_channels():
        print("Failed to fetch DM channels.")
        return None

    users = tool.get_all_dm_users()
    if not users:
        print("No DM users found.")
        return None

    print("\nSort by:")
    print("[1] Recent interaction (default)")
    print("[2] Alphabetical by username")
    sort_choice = input("Sort mode (1 or 2): ").strip()
    if sort_choice == '2':
        users.sort(key=lambda u: (u.get('username') or '').lower())
    else:
        users.sort(key=lambda u: int(u.get('last_message_id') or 0), reverse=True)

    def describe(user):
        display_name = user['global_name'] or user['username']
        return f"{display_name}#{user['discriminator']} (ID: {user['id']})"

    return _choose_numbered(users, describe, "Select user (number): ", "SELECT A USER")


def select_guild_channel(tool):
    """Prompt the user to pick a guild and channel. Returns (guild, channel) or None."""
    if not tool.get_guilds() or not tool.guilds:
        print("No servers found.")
        return None

    selected_guild = _choose_numbered(
        tool.guilds,
        lambda g: f"{g['name']} (ID: {g['id']})",
        "Select server (number): ",
        "SELECT A SERVER")
    if selected_guild is None:
        return None

    channels = tool.get_guild_channels(selected_guild['id'])
    if not channels:
        print("No text channels found.")
        return None

    selected_channel = _choose_numbered(
        channels,
        lambda c: f"#{c['name']} (ID: {c['id']})",
        "Select channel (number): ",
        f"SELECT A CHANNEL IN {selected_guild['name']}")
    if selected_channel is None:
        return None
    return selected_guild, selected_channel


def browse_archives(focus_channel_id=None, archive_dir=None):
    """Serve the archival folder in the local viewer and open it in the browser.
    With focus_channel_id, the viewer opens directly on that chat's archive.
    Blocks until the user presses Enter, then stops the server and returns."""
    archive_dir = resolve_archive_dir(archive_dir)
    select_name = None
    if focus_channel_id:
        focus = find_existing_archive(focus_channel_id, archive_dir)
        if focus and (focus / "messages.jsonl").exists():
            select_name = focus.name
    try:
        server, url = create_server(archive_dir, select_archive=select_name)
    except OSError as exc:
        print(f"Could not start the viewer: {exc}")
        return
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    print(f"\nViewer running at {url}")
    webbrowser.open(url)
    try:
        input("Press Enter to stop the viewer and return to the menu... ")
    except (KeyboardInterrupt, EOFError):
        pass
    server.shutdown()
    server.server_close()
    print("Viewer stopped.")


def pick_merge_target(channel_id, archive_dir):
    """Offer merging into an existing archive when the chat has no archive of
    its own. Returns the chosen folder path, or None to start a new archive.
    Returns False when the user cancels the archive run entirely."""
    if find_existing_archive(channel_id, archive_dir) is not None:
        return None  # this chat already has an archive: it resumes automatically
    others = list_archives(archive_dir)
    if not others:
        return None

    while True:
        print("\n" + "="*80)
        print("NO PREVIOUS ARCHIVE FOR THIS CHAT")
        print("="*80)
        print("[1] Start a new archive (default)")
        print("[2] Merge into an existing archive")
        print("[0] Cancel")

        try:
            choice = int(input("Select (0-2): ").strip())
        except ValueError:
            print("Invalid selection.")
            continue
        if choice == 0:
            return False
        if choice == 1:
            return None
        if choice != 2:
            print("Invalid selection.")
            continue
        break

    print("\n" + "="*80)
    print("MERGE INTO AN EXISTING ARCHIVE")
    print("="*80)
    for i, path in enumerate(others, 1):
        meta = read_meta(path)
        chat = meta.get("chat") or path.name
        channel = meta.get("channel_id") or "?"
        print(f"[{i}] {chat} (channel ID: {channel})")
    print("\n[0] Cancel")

    while True:
        try:
            choice = int(input("Select archive (number): ").strip())
        except ValueError:
            print("Invalid selection.")
            continue
        if choice == 0:
            return False
        if not 1 <= choice <= len(others):
            print("Invalid selection.")
            continue
        return others[choice - 1]


def handle_archive(tool, archive_dir=None):
    """Handle chat archiving (JSONL + attachments, nothing is deleted)."""
    archive_dir = resolve_archive_dir(archive_dir)
    print("\n" + "="*80)
    print("ARCHIVE A CHAT")
    print(f"Archival folder: {archive_dir}")
    print("="*80)
    print("[1] Archive a DM")
    print("[2] Archive a server channel")
    print("[0] Cancel")

    try:
        choice = int(input("Select (0-2): ").strip())
    except ValueError:
        print("Invalid selection.")
        return

    if choice == 1:
        selected_user = select_dm_user(tool)
        if not selected_user:
            return
        channel_id = selected_user['channel_id']
        label = selected_user['global_name'] or selected_user['username']
    elif choice == 2:
        selected = select_guild_channel(tool)
        if not selected:
            return
        guild, channel = selected
        channel_id = channel['id']
        label = f"{guild['name']} #{channel['name']}"
    else:
        return

    merge_target = pick_merge_target(str(channel_id), archive_dir)
    if merge_target is False:
        print("Cancelled.")
        return
    folder = archive_chat(tool, channel_id, label, archive_dir,
                          merge_target=merge_target)
    if folder and input("\nOpen this archive in the viewer now? (y/N): ").strip().lower() == 'y':
        browse_archives(focus_channel_id=channel_id, archive_dir=archive_dir)


def _parse_iso(value):
    """An ISO string from a saved session back into a datetime, or None."""
    if not value:
        return None
    try:
        return datetime.fromisoformat(value)
    except ValueError:
        return None


def describe_range(start_time, end_time):
    """Human-readable range for plans and resume prompts."""
    start = start_time.strftime("%Y-%m-%d %H:%M") if start_time else "the beginning"
    end = end_time.strftime("%Y-%m-%d %H:%M") if end_time else "now"
    return f"{start} to {end}"


def prompt_time_range():
    """Prompt for an optional start and end time."""
    print("\n" + "="*80)
    print("SELECT TIME RANGE")
    print("Leave blank for no limit; also accepted: 7d, 2w, today, yesterday")
    print("="*80)
    start_time = parse_datetime("Start date (YYYY-MM-DD HH:MM:SS): ")
    end_time = parse_datetime("End date (YYYY-MM-DD HH:MM:SS): ")
    return start_time, end_time


def handle_deletion(tool, channel_id, label, dry_run=False):
    """Shared DM/server deletion: offer to resume an unfinished run for this
    channel (one keypress, no rescanning), otherwise plan a new run, confirm
    it up front, then stream-scan and delete with live progress."""
    channel_id = str(channel_id)
    resume = None
    session = load_session(channel_id)

    if session:
        start_time = _parse_iso(session.get("start_time"))
        end_time = _parse_iso(session.get("end_time"))
        print("\n" + "="*80)
        print("UNFINISHED DELETION RUN FOR THIS CHAT")
        print("="*80)
        print(f"Deleted {session.get('deleted', 0)} of your messages so far "
              f"(scanned {session.get('scanned', 0)}, "
              f"skipped {session.get('skipped', 0)}, "
              f"failed {session.get('failed', 0)}).")
        print(f"Range: {describe_range(start_time, end_time)}")
        print("[1] Resume where it stopped (default)")
        print("[2] Start a new deletion run instead")
        print("[0] Cancel")

        while True:
            choice = (input("Select (0-2): ").strip() or "1")
            if choice == "0":
                return
            if choice == "1":
                resume = session
                break
            if choice == "2":
                start_time, end_time = prompt_time_range()
                break
            print("Invalid selection.")
    else:
        start_time, end_time = prompt_time_range()

    print("\n" + "="*80)
    print("DELETION PLAN")
    print(f"Channel: {label} (ID: {channel_id})")
    print("Scope: every message YOU sent in this channel")
    print(f"Range: {describe_range(start_time, end_time)}")
    print(f"Pace: ~24/min ({RATE_LIMIT_DELAY:.1f}s per delete); the scan runs live with progress")
    if dry_run:
        print("DRY RUN: nothing will be deleted")
    print("="*80)

    if input("Continue? (y/N): ").strip().lower() != 'y':
        print("Cancelled.")
        return

    summary = run_deletion(tool, channel_id, label,
                          start_time=start_time, end_time=end_time,
                          resume=resume, dry_run=dry_run)

    if summary["complete"]:
        if dry_run:
            print(f"\nDry run: {summary['deleted']} of your messages in range "
                  f"would be deleted ({summary['scanned']} messages scanned).")
            for sample in summary["samples"]:
                print(f"  {sample}")
        else:
            print(f"\nDone. Deleted {summary['deleted']}, failed {summary['failed']}, "
                  f"skipped {summary['skipped']} "
                  f"({summary['scanned']} messages scanned).")
            if summary["skipped"]:
                print("Skipped messages are system messages (calls, pin notifications) "
                      "that the platform does not allow deleting; they are filtered "
                      "out without a delete attempt.")
    else:
        print(f"\nDeleted {summary['deleted']} so far "
              f"(failed {summary['failed']}, skipped {summary['skipped']}, "
              f"{summary['scanned']} scanned).")
        print("Rerun the deletion for this chat to resume where it stopped.")


def handle_dm_deletion(tool, dry_run=False):
    """Handle DM message deletion."""
    selected_user = select_dm_user(tool)
    if not selected_user:
        return
    label = selected_user['global_name'] or selected_user['username']
    handle_deletion(tool, selected_user['channel_id'], f"DM with {label}",
                    dry_run=dry_run)


def handle_server_deletion(tool, dry_run=False):
    """Handle server message deletion."""
    selected = select_guild_channel(tool)
    if not selected:
        return
    selected_guild, selected_channel = selected
    label = f"{selected_guild['name']} #{selected_channel['name']}"
    handle_deletion(tool, selected_channel['id'], label, dry_run=dry_run)


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="message-guillotine",
        description="Delete and archive your DM and server messages "
                     "(self-bot — violates the platform ToS, use at your own risk).")
    parser.add_argument("--archive-dir", metavar="FOLDER",
                        help="archival folder for archives and the viewer "
                             "(overrides DM_ARCHIVE_DIR and the config file; "
                             "menu option [4] can still change it in-session)")
    parser.add_argument("--dry-run", action="store_true",
                        help="scan and report what would be deleted, without deleting")
    args = parser.parse_args(argv)

    print("="*80)
    print("MESSAGE GUILLOTINE")
    print("Delete messages from DMs or servers at 24/min rate")
    print("WARNING: Self-bots violate the platform ToS. Use at your own risk.")
    print("="*80)

    token = input("Enter your user token: ").strip()
    if not token:
        print("No token provided. Exiting.")
        return

    tool = MessageGuillotine(token)

    if not tool.get_current_user():
        print("Invalid token or authentication failed.")
        return

    # The --archive-dir override stays in effect until the user picks a
    # different folder in-session (menu option [4]), which then wins.
    cli_archive_dir = args.archive_dir
    while True:
        archive_dir = resolve_archive_dir(cli_archive_dir)
        print("\n" + "="*80)
        print("SELECT MODE")
        print(f"Archival folder: {archive_dir}")
        if args.dry_run:
            print("DRY RUN mode: nothing will be deleted")
        print("="*80)
        print("[1] Delete DM messages")
        print("[2] Delete server messages")
        print("[3] Archive a chat (JSONL + attachments, no deletion)")
        print("[4] Set archive folder")
        print("[5] Browse archives in the viewer")
        print("[0] Exit")

        try:
            raw = input("Mode (0-5): ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return
        if not raw.isdigit() or not 0 <= int(raw) <= 5:
            print("Invalid selection.")
            continue
        mode = int(raw)

        if mode == 0:
            return
        elif mode == 1:
            handle_dm_deletion(tool, dry_run=args.dry_run)
        elif mode == 2:
            handle_server_deletion(tool, dry_run=args.dry_run)
        elif mode == 3:
            handle_archive(tool, archive_dir=cli_archive_dir)
        elif mode == 4:
            new_dir = configure_archive_dir(archive_dir)
            if new_dir != archive_dir:
                cli_archive_dir = None  # the in-session choice replaces the CLI arg
        elif mode == 5:
            browse_archives(archive_dir=cli_archive_dir)
        # otherwise: loop back to the mode menu


if __name__ == "__main__":
    main()
