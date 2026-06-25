// 3D 数字孪生:three.js + urdf-loader 加载 firefly URDF,按 joint_state 实时更新关节角。
// TODO:目前 URDF+STL 捆在前端 public/urdf/(写死 firefly)。多型号后改成从机器人 arm/urdf
//       动态取(后端解析 package:// 供网格),见 zenoh_arm / 02-arm-api 的 UrdfResource。
import { useEffect, useRef } from "react";
import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import { STLLoader } from "three/addons/loaders/STLLoader.js";
import URDFLoader from "urdf-loader";
import type { URDFRobot } from "urdf-loader";

export function ArmViewer({ q, gravity, jointNames }: { q: number[]; gravity: [number, number, number]; jointNames: string[] }) {
  const mountRef = useRef<HTMLDivElement>(null);
  const robotRef = useRef<URDFRobot | null>(null);
  const arrowRef = useRef<THREE.ArrowHelper | null>(null);
  const autoJointsRef = useRef<string[]>([]);

  useEffect(() => {
    const mount = mountRef.current!;
    const H = 440;
    const W = mount.clientWidth || 600;
    const scene = new THREE.Scene();
    scene.background = new THREE.Color(0x1a1d23);
    const camera = new THREE.PerspectiveCamera(50, W / H, 0.01, 100);
    camera.position.set(0.7, -0.9, 0.7);
    camera.up.set(0, 0, 1); // URDF 是 Z-up
    const renderer = new THREE.WebGLRenderer({ antialias: true });
    renderer.setSize(W, H);
    renderer.setPixelRatio(window.devicePixelRatio);
    mount.appendChild(renderer.domElement);
    const controls = new OrbitControls(camera, renderer.domElement);
    controls.target.set(0, 0, 0.3);

    scene.add(new THREE.AmbientLight(0xffffff, 0.75));
    const dir = new THREE.DirectionalLight(0xffffff, 0.8);
    dir.position.set(1, 1, 2);
    scene.add(dir);
    scene.add(new THREE.GridHelper(2, 20, 0x444444, 0x2a2a2a).rotateX(Math.PI / 2)); // XY 平面(Z-up)

    // 重力箭头(从基座上方指向重力方向)
    const arrow = new THREE.ArrowHelper(new THREE.Vector3(0, 0, -1), new THREE.Vector3(0, 0, 0.5), 0.3, 0xff5555);
    scene.add(arrow);
    arrowRef.current = arrow;

    const loader = new URDFLoader();
    loader.packages = { xpkg_urdf_firefly_y6: "/urdf" };
    loader.loadMeshCb = (url, manager, onLoad) => {
      new STLLoader(manager).load(
        url,
        (geom) => onLoad(new THREE.Mesh(geom, new THREE.MeshPhongMaterial({ color: 0xbfc4cc }))),
        undefined,
        () => onLoad(new THREE.Object3D()),
      );
    };
    loader.loadAsync("/urdf/firefly.urdf").then((robot) => {
      robotRef.current = robot;
      autoJointsRef.current = Object.keys(robot.joints).filter((n) => (robot.joints[n] as any).jointType !== "fixed");
      scene.add(robot);
    }).catch((e) => console.error("URDF load failed", e));

    let raf = 0;
    const animate = () => { controls.update(); renderer.render(scene, camera); raf = requestAnimationFrame(animate); };
    animate();
    const onResize = () => {
      const w = mount.clientWidth || 600;
      camera.aspect = w / H; camera.updateProjectionMatrix(); renderer.setSize(w, H);
    };
    window.addEventListener("resize", onResize);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", onResize);
      renderer.dispose();
      if (renderer.domElement.parentNode === mount) mount.removeChild(renderer.domElement);
    };
  }, []);

  // 关节角更新
  useEffect(() => {
    const robot = robotRef.current;
    if (!robot) return;
    const names = jointNames.length ? jointNames : autoJointsRef.current;
    names.forEach((n, i) => { if (robot.joints[n]) robot.setJointValue(n, q[i] ?? 0); });
  }, [q, jointNames]);

  // 重力箭头方向/长度
  useEffect(() => {
    const arrow = arrowRef.current;
    if (!arrow) return;
    const g = new THREE.Vector3(gravity[0], gravity[1], gravity[2]);
    const len = g.length();
    if (len > 1e-6) {
      arrow.setDirection(g.clone().normalize());
      arrow.setLength(0.12 + 0.22 * Math.min(len / 9.81, 1), 0.05, 0.03);
    }
  }, [gravity]);

  return <div ref={mountRef} style={{ width: "100%", height: 440, borderRadius: 8, overflow: "hidden" }} />;
}
