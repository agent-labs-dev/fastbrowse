import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {DndContext, useDndContext, KeyboardSensor, PointerSensor, closestCenter, useSensor, useSensors} from '@dnd-kit/core';
import {SortableContext, arrayMove, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy} from '@dnd-kit/sortable';
import {CSS} from '@dnd-kit/utilities';

function Card({id}) {
  const {active} = useDndContext();
  const {attributes, listeners, setNodeRef, transform, transition} = useSortable({id});
  const style = {transform: CSS.Transform.toString(transform), transition, pointerEvents: active ? 'none' : undefined, padding: 12, margin: 6, border: '1px solid #888', background: '#fff'};
  return <button type="button" ref={setNodeRef} style={style} {...attributes} {...listeners}>{id}</button>;
}

function App() {
  const [items, setItems] = useState(['Test image', 'Second image', 'Third image']);
  const sensors = useSensors(useSensor(PointerSensor), useSensor(KeyboardSensor, {coordinateGetter: sortableKeyboardCoordinates}));
  const end = ({active, over}) => {
    if (over && active.id !== over.id) setItems(l => arrayMove(l, l.indexOf(active.id), l.indexOf(over.id)));
  };
  return <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={end}>
    <SortableContext items={items} strategy={verticalListSortingStrategy}>
      <div id="list" style={{display: 'flex', flexDirection: 'column', width: 300}}>
        {items.map(i => <Card key={i} id={i} />)}
      </div>
    </SortableContext>
    <output id="order">{items.join(',')}</output>
  </DndContext>;
}
createRoot(document.getElementById('root')).render(<App />);
