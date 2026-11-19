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
5. Same save for both: the host loads their save. The other player waits in the title menu until the
   log says "host's save written to slot N", then loads that slot (Load Game). Their own save in that
   slot is kept as SlotData_N.dat.nier_coop.bak in Documents\My Games\NieR_Automata.
   Every time the host saves, the new save is copied again (reload it to catch up).
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
