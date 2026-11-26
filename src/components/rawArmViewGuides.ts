import type { PerspectiveCamera, WebGLRenderer } from "three";
import { ViewHelper } from "three/addons/helpers/ViewHelper.js";

export function createRawArmViewGuides(renderer: WebGLRenderer, camera: PerspectiveCamera) {
  const axes = new ViewHelper(camera, renderer.domElement);
  axes.setLabelStyle("bold 23px sans-serif", "#101319", 24);
  axes.setLabels("+X", "+Y", "+Z");

  return {
    render() {
      const autoClear = renderer.autoClear;
      renderer.autoClear = false;
      axes.render(renderer);
      renderer.autoClear = autoClear;
    },
    dispose() { axes.dispose(); },
  };
}
