NieR:Automata co-op (nier_coop) - test build
==============================================

Install (both players)
1. Close the game. Unzip this folder anywhere.
2. Double-click Install.bat.
   - Answer "y" if you are the HOST, "n" if not. Exactly one of you is the host.
   - It shows your IP: send it to your friend, and type in the IP they send you.
3. Start NieR:Automata from Steam as usual. A black log window opens with it.
   DO NOT close that window: it closes the game.
4. If Windows Firewall asks about NieRAutomata, click Allow.
5. Same save for both: the HOST starts the game first. When the other player starts theirs, the host's
   save is copied in during the intro logos (the log says "host's save is in place"); then pick
   Load Game -> slot N (not Continue). If the host's game was not open yet, the logos wait up to 20 s;
   after that, restart the game to get the save.
   Their own save in that slot is kept as SlotData_N.dat.nier_coop.bak in Documents\My Games\NieR_Automata.
   When the host saves again, the other player gets it on their next game start.
6. Once both are in game: the log says "connected to ..." when you are connected,
   and the other player shows up as a second 2B.

Not connecting?
- Usually works without router setup. If not, the easiest fix: both install Tailscale
  (tailscale.com, free), sign in, and run Install.bat again using the 100.x.x.x IP Tailscale shows.
- Or the host forwards UDP port 27015 on their router to their PC.

After a test: send each other nier_coop.log (in the game folder). It is rewritten every launch,
so copy it before starting the game again.

Uninstall: double-click Uninstall.bat. Steam's "Verify integrity" does NOT remove the mod.

Known limits: flight-unit (mech) sections are not synced; enemy sync is experimental.
