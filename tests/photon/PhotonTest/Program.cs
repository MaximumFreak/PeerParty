// PeerParty Photon real-client test: Photon's own .NET client library (copies of a
// game's Photon3Unity3D / PhotonRealtime / PhotonChat; see ../run.ps1) against PeerParty's
// Photon servers. Run it under `peerparty.exe PhotonTest.exe`: the client looks
// up ns.photonengine.io as it would in a game, and the hook answers.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Threading;
using ExitGames.Client.Photon;
#if HAS_CHAT
using Photon.Chat;
#endif
using Photon.Realtime;
using Hashtable = ExitGames.Client.Photon.Hashtable;

class Cb : IConnectionCallbacks, IMatchmakingCallbacks, IInRoomCallbacks, IOnEventCallback, ILobbyCallbacks
{
    public string Name; public bool Master, Joined, InLobby, Left; public DisconnectCause? Disc;
    public List<RoomInfo> Rooms = new(); public List<EventData> Events = new(); public List<string> Notes = new();
    public Hashtable RoomChanged; public Player Entered, LeftPlayer, NewMaster; public Hashtable PlayerChanged;
    public string Failed;
    void N(string s) { Console.WriteLine($"{Name}: {s}"); Notes.Add(s); }
    public void OnConnected() => N("connected");
    public void OnConnectedToMaster() { Master = true; N("connected to master"); }
    public void OnDisconnected(DisconnectCause c) { Disc = c; N("disconnected " + c); }
    public void OnRegionListReceived(RegionHandler r) => N("regions " + r.EnabledRegions.Count);
    public void OnCustomAuthenticationResponse(Dictionary<string, object> d) { }
    public void OnCustomAuthenticationFailed(string m) => N("custom auth failed " + m);
    public void OnFriendListUpdate(List<FriendInfo> f) { }
    public void OnCreatedRoom() => N("created room");
    public void OnCreateRoomFailed(short c, string m) { Failed = $"create {c} {m}"; N(Failed); }
    public void OnJoinedRoom() { Joined = true; N("joined room"); }
    public void OnJoinRoomFailed(short c, string m) { Failed = $"join {c} {m}"; N(Failed); }
    public void OnJoinRandomFailed(short c, string m) { Failed = $"random {c} {m}"; N(Failed); }
    public void OnLeftRoom() { Left = true; N("left room"); }
    public void OnPlayerEnteredRoom(Player p) { Entered = p; N("player entered " + p.ActorNumber); }
    public void OnPlayerLeftRoom(Player p) { LeftPlayer = p; N("player left " + p.ActorNumber); }
    public void OnRoomPropertiesUpdate(Hashtable h) { RoomChanged = h; N("room props " + h.ToStringFull()); }
    public void OnPlayerPropertiesUpdate(Player p, Hashtable h) { PlayerChanged = h; N($"player {p.ActorNumber} props {h.ToStringFull()}"); }
    public void OnMasterClientSwitched(Player p) { NewMaster = p; N("master is now " + p.ActorNumber); }
    public void OnEvent(EventData e) { if (e.Code < 200) { Events.Add(e); N($"event {e.Code} from {e.Sender}"); } }
    public void OnJoinedLobby() { InLobby = true; N("joined lobby"); }
    public void OnLeftLobby() { }
    public void OnRoomListUpdate(List<RoomInfo> l) { Rooms = l; N("room list " + l.Count); }
    public void OnLobbyStatisticsUpdate(List<TypedLobbyInfo> s) { }
}

#if HAS_CHAT
class ChatCb : IChatClientListener
{
    public string Name = "";
    public bool Connected, Done;
    public readonly List<string> Channel = new(), Private = new();
    public bool Subscribed; public int FriendStatus = -1;
    public void DebugReturn(DebugLevel l, string m) { if (l <= DebugLevel.WARNING) Console.WriteLine($"chat {Name}: {m}"); }
    public void OnDisconnected() { Done = true; Console.WriteLine($"chat {Name}: disconnected"); }
    public void OnConnected() { Connected = true; Console.WriteLine($"chat {Name}: connected"); }
    public void OnChatStateChange(ChatState s) { }
    public void OnGetMessages(string c, string[] s, object[] m) { for (int i = 0; i < m.Length; i++) Channel.Add($"{c}:{s[i]}:{m[i]}"); }
    public void OnPrivateMessage(string s, object m, string c) { Private.Add($"{s}:{m}"); }
    public void OnSubscribed(string[] c, bool[] r) { Subscribed = r.Length > 0 && r[0]; }
    public void OnUnsubscribed(string[] c) { }
    public void OnStatusUpdate(string u, int s, bool g, object m) { FriendStatus = s; }
    public void OnUserSubscribed(string c, string u) { }
    public void OnUserUnsubscribed(string c, string u) { }
}
#endif

static class Program
{
    const string App = "7b1e5f2a-0c3d-4e8f-9a6b-1c2d3e4f5a6b";
    static string Host = Environment.GetEnvironmentVariable("LB_PHOTON_HOST") ?? "ns.photonengine.io";
    static readonly List<LoadBalancingClient> All = new();
    static readonly List<Cb> Cbs = new();
    // Photon's WebSocket client class, in the builds that have one.
    // By name: listing an assembly's types throws when some refer to Unity types not here.
    static readonly Type WsClient = typeof(PhotonPeer).Assembly.GetType("ExitGames.Client.Photon.PhotonClientWebSocket")
        ?? typeof(LoadBalancingClient).Assembly.GetType("Photon.Realtime.PhotonClientWebSocket")
        ?? typeof(LoadBalancingClient).Assembly.GetType("ExitGames.Client.Photon.PhotonClientWebSocket");

    static void Pump(Func<bool> done, string what, int ms = 15000)
    {
        var sw = Stopwatch.StartNew();
        while (!done())
        {
            foreach (var c in All) c.Service();
            if (sw.ElapsedMilliseconds > ms) throw new Exception("timed out waiting for " + what);
            Thread.Sleep(5);
        }
    }

    static void Check(bool ok, string what) { if (!ok) throw new Exception("check failed: " + what); Console.WriteLine("ok: " + what); }

    static (LoadBalancingClient, Cb) Connect(string name, string user, SerializationProtocol proto, AuthModeOption auth, ConnectionProtocol transport = ConnectionProtocol.Udp)
    {
        var c = new LoadBalancingClient();
        c.SerializationProtocol = proto;
        c.AuthMode = auth;
        c.AuthValues = new Photon.Realtime.AuthenticationValues(user);
        c.NickName = name;
        var cb = new Cb { Name = name };
        c.AddCallbackTarget(cb);
        All.Add(c);
        // Photon's own WebSocket client (System.Net.WebSockets), as a game outside Unity's
        // WebGL would set it.
        if (WsClient != null) c.LoadBalancingPeer.SocketImplementationConfig[ConnectionProtocol.WebSocket] = WsClient;
        Cbs.Add(cb);
        var s = new AppSettings { AppIdRealtime = App, AppVersion = "1.0", FixedRegion = "eu", Server = Host, Protocol = transport, UseNameServer = true, EnableProtocolFallback = false };
        Check(c.ConnectUsingSettings(s), $"{name} starts connecting to {Host} ({proto}, {auth})");
        Pump(() => cb.Master || cb.Disc != null, name + " master");
        Check(cb.Master, $"{name} reached the master server");
        return (c, cb);
    }

    static int Main()
    {
        try
        {
            var (a, ca) = Connect("A", "alice", SerializationProtocol.GpBinaryV16, AuthModeOption.Auth);
            var opts = new RoomOptions { MaxPlayers = 4, PublishUserId = true, CustomRoomProperties = new Hashtable { { "map", "forest" } }, CustomRoomPropertiesForLobby = new[] { "map" } };
            Check(a.OpCreateRoom(new EnterRoomParams { RoomName = "pproom", RoomOptions = opts }), "A sends CreateRoom");
            Pump(() => ca.Joined || ca.Failed != null, "A in room");
            Check(ca.Joined && a.LocalPlayer.ActorNumber == 1 && a.LocalPlayer.IsMasterClient, "A created pproom as actor 1, master client");

            var (b, cbb) = Connect("B", "bob", SerializationProtocol.GpBinaryV18, AuthModeOption.AuthOnce);
            Check(b.OpJoinLobby(TypedLobby.Default), "B sends JoinLobby");
            Pump(() => cbb.Rooms.Exists(r => r.Name == "pproom"), "pproom in B's room list");
            var info = cbb.Rooms.Find(r => r.Name == "pproom");
            Check(info.PlayerCount == 1 && info.MaxPlayers == 4 && (string)info.CustomProperties["map"] == "forest", "lobby shows pproom 1/4 with map=forest");
            Check(b.OpJoinRandomRoom(new OpJoinRandomRoomParams { ExpectedCustomRoomProperties = new Hashtable { { "map", "forest" } } }), "B sends JoinRandomRoom");
            Pump(() => cbb.Joined || cbb.Failed != null, "B in room");
            Check(cbb.Joined && b.CurrentRoom.Name == "pproom" && b.CurrentRoom.PlayerCount == 2, "B joined pproom by random match");
            Check(b.CurrentRoom.GetPlayer(1)?.UserId == "alice" && b.CurrentRoom.GetPlayer(1)?.NickName == "A", "B sees A's user id and nickname");
            Pump(() => ca.Entered != null, "A sees B enter");

            Check(b.OpRaiseEvent(1, new object[] { "hello", 42, new float[] { 1.5f, 2.5f } }, RaiseEventOptions.Default, SendOptions.SendReliable), "B raises event 1");
            Pump(() => ca.Events.Exists(e => e.Code == 1), "A gets event 1");
            var e1 = ca.Events.Find(e => e.Code == 1); var d1 = (object[])e1.CustomData;
            Check(e1.Sender == 2 && (string)d1[0] == "hello" && (int)d1[1] == 42, "event 1 arrives from actor 2 with its data");
            var big = new byte[5000]; new Random(1).NextBytes(big);
            a.OpRaiseEvent(2, big, RaiseEventOptions.Default, SendOptions.SendReliable);
            Pump(() => cbb.Events.Exists(e => e.Code == 2), "B gets big event 2");
            Check(((byte[])cbb.Events.Find(e => e.Code == 2).CustomData).AsSpan().SequenceEqual(big), "5000-byte event arrives intact (fragmented)");
            a.OpRaiseEvent(3, 7, RaiseEventOptions.Default, SendOptions.SendUnreliable);
            Pump(() => cbb.Events.Exists(e => e.Code == 3), "B gets unreliable event 3");

            a.CurrentRoom.SetCustomProperties(new Hashtable { { "round", 2 } });
            Pump(() => cbb.RoomChanged != null && cbb.RoomChanged.ContainsKey("round"), "B sees room property");
            Check((int)b.CurrentRoom.CustomProperties["round"] == 2, "room property round=2 reaches B");
            b.LocalPlayer.SetCustomProperties(new Hashtable { { "score", 5 } });
            Pump(() => ca.PlayerChanged != null && ca.PlayerChanged.ContainsKey("score"), "A sees B's player property");

            a.OpRaiseEvent(5, "cached", new RaiseEventOptions { CachingOption = EventCaching.AddToRoomCache }, SendOptions.SendReliable);
            Pump(() => cbb.Events.Exists(e => e.Code == 5), "B gets event 5");
            var (c, cc) = Connect("C", "carol", SerializationProtocol.GpBinaryV18, AuthModeOption.Auth);
            Check(c.OpJoinRoom(new EnterRoomParams { RoomName = "pproom" }), "C sends JoinRoom");
            Pump(() => cc.Joined || cc.Failed != null, "C in room");
            Check(cc.Joined && c.CurrentRoom.PlayerCount == 3, "C joined pproom by name");
            Pump(() => cc.Events.Exists(e => e.Code == 5), "C gets the cached event");
            Check((string)cc.Events.Find(e => e.Code == 5).CustomData == "cached", "late joiner gets the cached event");

            Check(a.OpLeaveRoom(false), "A leaves");
            Pump(() => cbb.LeftPlayer != null && cc.LeftPlayer != null && cbb.NewMaster != null, "B and C see A leave");
            Check(b.LocalPlayer.IsMasterClient && cbb.NewMaster.ActorNumber == 2, "B became master client");
            // Photon's TCP transport: a client that joins the same room over TCP.
            var (d, cd) = Connect("D", "dave", SerializationProtocol.GpBinaryV18, AuthModeOption.Auth, ConnectionProtocol.Tcp);
            Check(d.OpJoinRoom(new EnterRoomParams { RoomName = "pproom" }), "D (TCP) sends JoinRoom");
            Pump(() => cd.Joined || cd.Failed != null, "D in room");
            Check(cd.Joined && d.CurrentRoom.PlayerCount == 3, "D joined pproom over TCP");
            b.OpRaiseEvent(6, "over tcp", RaiseEventOptions.Default, SendOptions.SendReliable);
            Pump(() => cd.Events.Exists(e => e.Code == 6), "D gets event 6 over TCP");
            if (WsClient != null)
            {
                // Photon's WebSocket transport: one client without a token anywhere (Auth), one
                // that carries its token to the master and game servers (AuthOnce).
                var (ew, cew) = Connect("E", "erin", SerializationProtocol.GpBinaryV16, AuthModeOption.Auth, ConnectionProtocol.WebSocket);
                Check(ew.OpJoinRoom(new EnterRoomParams { RoomName = "pproom" }), "E (WebSocket) sends JoinRoom");
                Pump(() => cew.Joined || cew.Failed != null, "E in room");
                Check(cew.Joined && ew.CurrentRoom.PlayerCount == 4, "E joined pproom over WebSocket");
                b.OpRaiseEvent(7, "over websocket", RaiseEventOptions.Default, SendOptions.SendReliable);
                Pump(() => cew.Events.Exists(x => x.Code == 7), "E gets event 7 over WebSocket");
                ew.OpRaiseEvent(8, new byte[3000], RaiseEventOptions.Default, SendOptions.SendReliable);
                Pump(() => cd.Events.Exists(x => x.Code == 8), "D (TCP) gets E's 3000-byte event");
                var (f, cf) = Connect("F", "fay", SerializationProtocol.GpBinaryV18, AuthModeOption.AuthOnce, ConnectionProtocol.WebSocket);
                Check(f.OpCreateRoom(new EnterRoomParams { RoomName = "wsroom", RoomOptions = new RoomOptions { MaxPlayers = 2 } }), "F (WebSocket, AuthOnce) sends CreateRoom");
                Pump(() => cf.Joined || cf.Failed != null, "F in room");
                Check(cf.Joined && f.CurrentRoom.Name == "wsroom", "F created wsroom over WebSocket with its token");
            }
            else Console.WriteLine("no WebSocket client in this Photon build: WebSocket left out");
            // An SQL lobby: random matching by a WHERE clause over the rooms' lobby properties.
            var sql = new TypedLobby("sqlarena", LobbyType.SqlLobby);
            var (g, cg) = Connect("G", "gil", SerializationProtocol.GpBinaryV18, AuthModeOption.Auth);
            var gopts = new RoomOptions { MaxPlayers = 4, CustomRoomProperties = new Hashtable { { "C0", 5 }, { "C1", "Map2" } }, CustomRoomPropertiesForLobby = new[] { "C0", "C1" } };
            Check(g.OpCreateRoom(new EnterRoomParams { RoomName = "sqlroom", RoomOptions = gopts, Lobby = sql }), "G creates sqlroom in an SQL lobby");
            Pump(() => cg.Joined || cg.Failed != null, "G in sqlroom");
            var (h, ch) = Connect("H", "hal", SerializationProtocol.GpBinaryV18, AuthModeOption.Auth);
            Check(h.OpJoinRandomRoom(new OpJoinRandomRoomParams { TypedLobby = sql, SqlLobbyFilter = "C0 BETWEEN 1 AND 10 AND C1 = 'Map3'" }), "H asks for C1 = 'Map3'");
            Pump(() => ch.Joined || ch.Failed != null, "H's first random join answered");
            Check(!ch.Joined && ch.Failed != null, "no room passes C1 = 'Map3'");
            ch.Failed = null;
            Pump(() => h.IsConnectedAndReady && h.Server == ServerConnection.MasterServer, "H back on the master server");
            Check(h.OpJoinRandomRoom(new OpJoinRandomRoomParams { TypedLobby = sql, SqlLobbyFilter = "C0 BETWEEN 1 AND 10 AND C1 LIKE 'map%'" }), "H asks for C0 in 1..10 and any map");
            Pump(() => ch.Joined || ch.Failed != null, "H in a room");
            Check(ch.Joined && h.CurrentRoom.Name == "sqlroom", "H joined sqlroom through the SQL filter");
            // PlayerTTL: a player whose connection drops stays in the room, inactive, and
            // comes back as the same actor with ReconnectAndRejoin.
            var (k, ck) = Connect("K", "kim", SerializationProtocol.GpBinaryV18, AuthModeOption.AuthOnce);
            Check(k.OpCreateRoom(new EnterRoomParams { RoomName = "ttlroom", RoomOptions = new RoomOptions { MaxPlayers = 2, PlayerTtl = 60000, PublishUserId = true } }), "K creates ttlroom with a PlayerTTL");
            Pump(() => ck.Joined || ck.Failed != null, "K in ttlroom");
            var (l, cl) = Connect("L", "lee", SerializationProtocol.GpBinaryV18, AuthModeOption.AuthOnce);
            Check(l.OpJoinRoom(new EnterRoomParams { RoomName = "ttlroom" }), "L sends JoinRoom");
            Pump(() => cl.Joined || cl.Failed != null, "L in ttlroom");
            int lNr = l.LocalPlayer.ActorNumber;
            l.Disconnect();
            Pump(() => ck.LeftPlayer != null, "K sees L go");
            Check(ck.LeftPlayer.ActorNumber == lNr && ck.LeftPlayer.IsInactive && k.CurrentRoom.PlayerCount == 2, "L is inactive, still in the room");
            Pump(() => cl.Disc != null, "L disconnected");
            cl.Joined = false; cl.Disc = null; ck.Entered = null;
            Check(l.ReconnectAndRejoin(), "L reconnects and rejoins");
            Pump(() => cl.Joined || cl.Failed != null, "L back in ttlroom");
            Check(cl.Joined && l.LocalPlayer.ActorNumber == lNr, "L rejoined as the same actor");
            Pump(() => ck.Entered != null, "K sees L return");
            Check(!ck.Entered.IsInactive, "L is active again for K");
            foreach (var x in All) x.Disconnect();
            Pump(() => Cbs.TrueForAll(x => x.Disc != null), "disconnects", 5000);

#if HAS_CHAT
            // Photon Chat: two clients on PeerParty's chat front end.
            var chats = new List<ChatClient>();
            ChatClient ChatConnect(ChatCb cb, string user)
            {
                var cc = new ChatClient(cb) { AuthValues = new Photon.Chat.AuthenticationValues(user) };
                chats.Add(cc);
                cc.ConnectUsingSettings(new ChatAppSettings { AppIdChat = App, AppVersion = "1.0", FixedRegion = "eu", Server = Host, Protocol = ConnectionProtocol.Udp, EnableProtocolFallback = false });
                return cc;
            }
            void ChatPump(Func<bool> done, string what)
            {
                var sw = Stopwatch.StartNew();
                while (!done())
                {
                    foreach (var x in chats) x.Service();
                    if (sw.ElapsedMilliseconds > 15000) throw new Exception("timed out waiting for " + what);
                    Thread.Sleep(5);
                }
            }
            var ann = new ChatCb { Name = "ann" }; var bob = new ChatCb { Name = "bob" };
            var chatA = ChatConnect(ann, "ann"); var chatB = ChatConnect(bob, "bob");
            ChatPump(() => ann.Connected && bob.Connected, "chat clients connected");
            chatA.Subscribe(new[] { "global" }); chatB.Subscribe(new[] { "global" });
            ChatPump(() => ann.Subscribed && bob.Subscribed, "chat subscriptions");
            Check(chatA.PublishMessage("global", "hello"), "ann publishes on global");
            ChatPump(() => bob.Channel.Contains("global:ann:hello"), "bob gets ann's message");
            Check(chatB.SendPrivateMessage("ann", "hi ann"), "bob sends ann a private message");
            ChatPump(() => ann.Private.Contains("bob:hi ann"), "ann gets bob's private message");
            chatA.AddFriends(new[] { "bob" });
            ChatPump(() => ann.FriendStatus == ChatUserStatus.Online, "ann sees bob online");
            Check(true, "Photon Chat: channel, private message and friend status over peerparty");
            foreach (var x in chats) x.Disconnect();
#else
            Console.WriteLine("no PhotonChat.dll: Chat left out");
#endif

            Console.WriteLine("PHOTON_TEST_OK");
            return 0;
        }
        catch (Exception ex)
        {
            Console.WriteLine("PHOTON_TEST_FAIL " + ex);
            return 1;
        }
    }
}
