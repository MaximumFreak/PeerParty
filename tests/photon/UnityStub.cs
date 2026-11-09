namespace UnityEngine
{
    // The few Unity types Photon's Unity build of its .NET client touches outside a game
    // (logging, custom-type registration, ConnectionHandler's status fields), so it runs
    // in a plain .NET test host. Compiled as both UnityEngine and UnityEngine.CoreModule,
    // the two assembly names Photon3Unity3D and PhotonRealtime reference. Test only.
    public static class Debug
    {
        public static void Log(object m) => System.Console.WriteLine("[photon] " + m);
        public static void LogWarning(object m) => System.Console.WriteLine("[photon warn] " + m);
        public static void LogError(object m) => System.Console.WriteLine("[photon error] " + m);
    }
    public struct Vector2 { public float x, y; }
    public struct Vector3 { public float x, y, z; }
    public struct Quaternion { public float x, y, z, w; public static Quaternion identity => new Quaternion { w = 1 }; }
}

namespace UnityEngine
{
    public class Object { public static void DontDestroyOnLoad(Object o) { } public static void Destroy(Object o) { } }
    public class Component : Object { public GameObject gameObject => new GameObject("stub"); }
    public class Behaviour : Component { public bool enabled = true; }
    public class YieldInstruction { }
    public sealed class Coroutine : YieldInstruction { }
    public sealed class WaitForSeconds : YieldInstruction { public WaitForSeconds(float s) { } }
    public class MonoBehaviour : Behaviour { public Coroutine StartCoroutine(System.Collections.IEnumerator e) => new Coroutine(); }
    public sealed class GameObject : Object
    {
        public GameObject(string name) { }
        public T AddComponent<T>() where T : Component, new() => new T();
    }
    public enum NetworkReachability { NotReachable, ReachableViaCarrierDataNetwork, ReachableViaLocalAreaNetwork }
    public static class Application { public static NetworkReachability internetReachability => NetworkReachability.ReachableViaLocalAreaNetwork; }
}
